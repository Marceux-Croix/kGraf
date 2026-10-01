//! Pre-tool approval, bound to execution.
//!
//! The decision vocabulary (allow, ask, deny) follows goose `permission/`, which the
//! approval stream vendors behind this seam (0055). kGraf adds **rewrite**: a hook
//! may change a call's arguments, and that change is recorded, never silent (fail loud).
//!
//! **The only way to run a tool.** [`ApprovedCall`] has no public constructor, no
//! `Clone` and no `Default`. Only [`ApprovalChain::review`] and
//! [`ApprovalChain::answer`] make one, after writing every decision to an
//! [`ApprovalLog`]; if the log refuses a write, no approval is issued.
//! `ToolRegistry::execute` takes an `ApprovedCall` by value and is the only code that
//! makes the `CallPermit` [`crate::Tool::call`] requires.
//!
//! **Order of checks.** Hooks run in order, each seeing the input as the previous
//! hooks left it. The first deny ends the chain; an ask pauses it. After the last
//! hook, every [`CallPolicy`] (a skill's `allowed-tools`) checks the **final** input,
//! so a rewrite cannot turn an allowed call into a refused one unnoticed. Last, the
//! tool's [`crate::Access`] is computed from the final input and carried on the
//! approval, so the runtime schedules what will actually run.
//!
//! **An empty chain is recorded too**, as [`RecordedDecision::AllowedNoHooks`], so the
//! log shows that a call ran unreviewed rather than showing nothing.
//!
//! **Paused calls are sealed.** A [`PausedCall`] is serializable (a paused worker
//! resumes from its transcript) and its fields are private. It carries the digest of
//! the chain that paused it and a keyed blake3 seal over every field, the exact
//! input bytes and the resume position included. [`ApprovalChain::answer`] refuses a
//! call whose seal does not verify, or that a different chain paused. The seal key
//! defaults to a per-process random key, so a paused call cannot be forged or edited
//! at rest; a runtime that must resume a paused call across a restart supplies a
//! persisted key through [`ApprovalChain::with_seal_key`], or re-reviews the call.
//!
//! **Unattended runs (0052).** A scheduled run has nobody to answer an ask.
//! [`AskPolicy::Unattended`] denies an ask at once, or after a timeout, and records
//! the denial.

use crate::access::Access;
use crate::error::ApprovalError;
use crate::name::QualifiedName;
use crate::registry::ToolView;
use kgraf_core::{AgentId, TaskId};
use serde::{Deserialize, Serialize};
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::Duration;

/// A tool call awaiting approval: the full call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingCall {
    /// The model's id for this call.
    pub call_id: String,
    /// The tool's wire name.
    pub tool: String,
    /// The arguments.
    pub input: serde_json::Value,
    pub worker: AgentId,
    pub task: TaskId,
}

/// What one hook decides.
#[derive(Clone, Debug, PartialEq)]
pub enum HookDecision {
    /// Let the call proceed to the next hook.
    Allow,
    /// Pause the worker and ask the user.
    Ask { question: String },
    /// Refuse the call.
    Deny { reason: String },
    /// Replace the arguments. Recorded with the original and the replacement.
    Rewrite {
        input: serde_json::Value,
        reason: String,
    },
}

/// One pre-tool hook.
#[async_trait::async_trait]
pub trait ApprovalHook: Send + Sync {
    /// A stable name, recorded with each decision and part of the chain digest.
    fn name(&self) -> &str;

    /// Decide on the call.
    async fn review(&self, call: &PendingCall) -> HookDecision;
}

/// A restriction checked against the final input, after every hook.
///
/// `kgraf-session`'s `ToolPolicy` (a skill's `allowed-tools`) implements it.
pub trait CallPolicy: Send + Sync {
    /// A stable name, recorded with a refusal.
    fn name(&self) -> &str;

    /// A description of the policy's content, part of the chain digest, so a
    /// policy that changes while a call is paused is detected.
    fn fingerprint(&self) -> String;

    /// Permit or refuse, with the reason.
    fn check(&self, tool: &QualifiedName, subject: Option<&str>) -> Result<(), String>;
}

/// A policy refused an approved call when it started
/// ([`ApprovalChain::recheck`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyRefusal {
    /// The policy that refused, or `registry` for a tool no longer registered.
    pub by: String,
    pub reason: String,
}

/// What happens to an ask when nobody is there to answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskPolicy {
    /// A user is present; an ask pauses the call until answered.
    Interactive,
    /// A scheduled or background run (0052).
    Unattended { on_ask: UnattendedAsk },
}

/// An unattended run's handling of an ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnattendedAsk {
    /// Deny at once.
    Deny,
    /// Wait this long for an answer, then deny.
    TimeoutThenDeny { after: Duration },
}

/// A decision as recorded.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum RecordedDecision {
    Allowed,
    /// The chain has no hooks; the call proceeded unreviewed.
    AllowedNoHooks,
    Asked {
        question: String,
        /// When an unattended ask is denied, in epoch milliseconds.
        deadline_ms: Option<i64>,
    },
    Denied {
        reason: String,
    },
    Rewritten {
        original: serde_json::Value,
        rewritten: serde_json::Value,
        reason: String,
    },
    UserAllowed,
    UserDenied {
        reason: String,
    },
    /// An unattended ask reached its deadline unanswered.
    AskExpired,
}

/// One recorded decision.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub call_id: String,
    pub tool: String,
    /// The hook, policy or component that decided, or `user`.
    pub decided_by: String,
    pub decision: RecordedDecision,
}

/// Where decisions are recorded. The runtime implements this over the worker's
/// event stream, so the decisions are part of the transcript.
#[async_trait::async_trait]
pub trait ApprovalLog: Send {
    /// Record one decision. An error withholds the approval.
    async fn record(&mut self, record: ApprovalRecord) -> Result<(), ApprovalError>;
}

/// A call cleared to run. Only [`ApprovalChain`] makes one; only
/// `ToolRegistry::execute` consumes one.
#[derive(Debug, PartialEq)]
pub struct ApprovedCall {
    call_id: String,
    tool: String,
    input: serde_json::Value,
    access: Access,
    rewritten: bool,
}

impl ApprovedCall {
    /// The model's id for this call.
    pub fn call_id(&self) -> &str {
        &self.call_id
    }

    /// The tool's wire name.
    pub fn tool(&self) -> &str {
        &self.tool
    }

    /// The arguments it will run with, after any recorded rewrite.
    pub fn input(&self) -> &serde_json::Value {
        &self.input
    }

    /// What the call touches, computed from the final input.
    pub fn access(&self) -> &Access {
        &self.access
    }

    /// Whether any hook rewrote the arguments.
    pub fn was_rewritten(&self) -> bool {
        self.rewritten
    }

    pub(crate) fn into_input(self) -> serde_json::Value {
        self.input
    }
}

/// A call paused on an ask. Serializable for resume; sealed against change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PausedCall {
    call: PendingCall,
    asked_by: String,
    question: String,
    next_hook: usize,
    rewritten: bool,
    deadline_ms: Option<i64>,
    chain_digest: String,
    seal: String,
}

impl PausedCall {
    /// The call, as it stood when the ask was raised.
    pub fn call(&self) -> &PendingCall {
        &self.call
    }

    /// The hook that asked.
    pub fn asked_by(&self) -> &str {
        &self.asked_by
    }

    /// The question for the user.
    pub fn question(&self) -> &str {
        &self.question
    }

    /// When an unattended ask is denied, in epoch milliseconds.
    pub fn deadline_ms(&self) -> Option<i64> {
        self.deadline_ms
    }
}

/// The user's answer to an ask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UserAnswer {
    Allow,
    Deny { reason: String },
}

/// The outcome of review.
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Run(ApprovedCall),
    Ask(PausedCall),
    Denied { by: String, reason: String },
}

type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The ordered hooks and policies for one worker.
#[derive(Clone)]
pub struct ApprovalChain {
    hooks: Vec<Arc<dyn ApprovalHook>>,
    policies: Vec<Arc<dyn CallPolicy>>,
    ask_policy: AskPolicy,
    clock: Clock,
    seal_key: [u8; 32],
}

impl ApprovalChain {
    /// A chain of the given hooks, run in order. Interactive, no policies, a
    /// per-process random seal key.
    pub fn new(hooks: Vec<Arc<dyn ApprovalHook>>) -> Self {
        Self {
            hooks,
            policies: Vec::new(),
            ask_policy: AskPolicy::Interactive,
            clock: Arc::new(now_ms),
            seal_key: process_seal_key(),
        }
    }

    /// Add a policy checked against the final input.
    pub fn with_policy(mut self, policy: Arc<dyn CallPolicy>) -> Self {
        self.policies.push(policy);
        self
    }

    /// Set the ask policy.
    pub fn with_ask_policy(mut self, ask_policy: AskPolicy) -> Self {
        self.ask_policy = ask_policy;
        self
    }

    /// Replace the clock (epoch milliseconds), for tests and simulated time.
    pub fn with_clock(mut self, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Use a persisted seal key, so a paused call can be answered after a restart.
    pub fn with_seal_key(mut self, key: [u8; 32]) -> Self {
        self.seal_key = key;
        self
    }

    /// A digest of the chain's hooks, policies and ask policy.
    pub fn digest(&self) -> String {
        let mut h = blake3::Hasher::new();
        for hook in &self.hooks {
            field(&mut h, b"hook");
            field(&mut h, hook.name().as_bytes());
        }
        for policy in &self.policies {
            field(&mut h, b"policy");
            field(&mut h, policy.name().as_bytes());
            field(&mut h, policy.fingerprint().as_bytes());
        }
        field(&mut h, format!("{:?}", self.ask_policy).as_bytes());
        h.finalize().to_hex().to_string()
    }

    /// Check an approved call against every policy again, on the input it will
    /// run with. The runtime calls this when the call starts, because a policy
    /// can narrow between approval and start: every call of a round is approved
    /// before any runs, so a skill activated by an earlier call of the same
    /// round narrows the policy after a later call was approved (decision 0053).
    /// A chain with no policies checks nothing.
    pub fn recheck(
        &self,
        registry: &crate::registry::ToolRegistry,
        call: &ApprovedCall,
    ) -> Result<(), PolicyRefusal> {
        if self.policies.is_empty() {
            return Ok(());
        }
        let Some(registered) = registry.get(&call.tool) else {
            return Err(PolicyRefusal {
                by: "registry".into(),
                reason: format!("no tool named `{}`", call.tool),
            });
        };
        let subject = registered.tool().permission_subject(&call.input);
        for policy in &self.policies {
            policy
                .check(registered.name(), subject.as_deref())
                .map_err(|reason| PolicyRefusal {
                    by: policy.name().to_owned(),
                    reason,
                })?;
        }
        Ok(())
    }

    /// Review a call from the first hook.
    pub async fn review(
        &self,
        view: ToolView<'_>,
        call: PendingCall,
        log: &mut dyn ApprovalLog,
    ) -> Result<Verdict, ApprovalError> {
        self.run_from(view, 0, call, false, log).await
    }

    /// Continue a paused call with the user's answer, after verifying the seal.
    pub async fn answer(
        &self,
        view: ToolView<'_>,
        paused: PausedCall,
        answer: UserAnswer,
        log: &mut dyn ApprovalLog,
    ) -> Result<Verdict, ApprovalError> {
        self.verify(&paused)?;
        if let Some(denied) = self.deny_if_expired(&paused, log).await? {
            return Ok(denied);
        }
        let decision = match &answer {
            UserAnswer::Allow => RecordedDecision::UserAllowed,
            UserAnswer::Deny { reason } => RecordedDecision::UserDenied {
                reason: reason.clone(),
            },
        };
        log.record(record(&paused.call, "user", decision)).await?;
        match answer {
            UserAnswer::Allow => {
                self.run_from(view, paused.next_hook, paused.call, paused.rewritten, log)
                    .await
            }
            UserAnswer::Deny { reason } => Ok(Verdict::Denied {
                by: "user".into(),
                reason,
            }),
        }
    }

    /// For an unattended ask: deny it if its deadline has passed, otherwise hand
    /// it back unchanged. The runtime calls this when its wait times out.
    pub async fn expire(
        &self,
        paused: PausedCall,
        log: &mut dyn ApprovalLog,
    ) -> Result<Verdict, ApprovalError> {
        self.verify(&paused)?;
        match self.deny_if_expired(&paused, log).await? {
            Some(denied) => Ok(denied),
            None => Ok(Verdict::Ask(paused)),
        }
    }

    fn verify(&self, paused: &PausedCall) -> Result<(), ApprovalError> {
        if paused.chain_digest != self.digest() {
            return Err(ApprovalError::ChainChanged {
                call_id: paused.call.call_id.clone(),
            });
        }
        let expected = self.seal(
            &paused.call,
            &paused.asked_by,
            &paused.question,
            paused.next_hook,
            paused.rewritten,
            paused.deadline_ms,
            &paused.chain_digest,
        );
        if paused.seal != expected || paused.next_hook > self.hooks.len() {
            return Err(ApprovalError::PausedCallAltered {
                call_id: paused.call.call_id.clone(),
            });
        }
        Ok(())
    }

    async fn deny_if_expired(
        &self,
        paused: &PausedCall,
        log: &mut dyn ApprovalLog,
    ) -> Result<Option<Verdict>, ApprovalError> {
        match paused.deadline_ms {
            Some(deadline) if (self.clock)() >= deadline => {
                log.record(record(
                    &paused.call,
                    ASK_POLICY,
                    RecordedDecision::AskExpired,
                ))
                .await?;
                Ok(Some(Verdict::Denied {
                    by: ASK_POLICY.into(),
                    reason: "unattended run: the ask timed out unanswered".into(),
                }))
            }
            _ => Ok(None),
        }
    }

    async fn run_from(
        &self,
        view: ToolView<'_>,
        start: usize,
        mut call: PendingCall,
        mut rewritten: bool,
        log: &mut dyn ApprovalLog,
    ) -> Result<Verdict, ApprovalError> {
        let registered = match view.resolve(&call.tool) {
            Ok(r) => r,
            Err(e) => return deny(log, &call, "registry", e.to_string()).await,
        };
        if self.hooks.is_empty() {
            log.record(record(&call, "chain", RecordedDecision::AllowedNoHooks))
                .await?;
        }
        for (index, hook) in self.hooks.iter().enumerate().skip(start) {
            let by = hook.name().to_owned();
            match hook.review(&call).await {
                HookDecision::Allow => {
                    log.record(record(&call, &by, RecordedDecision::Allowed))
                        .await?;
                }
                HookDecision::Rewrite { input, reason } => {
                    let decision = RecordedDecision::Rewritten {
                        original: call.input.clone(),
                        rewritten: input.clone(),
                        reason,
                    };
                    // Recorded before the input is replaced: a failed write leaves
                    // the call unapproved, never running on unrecorded arguments.
                    log.record(record(&call, &by, decision)).await?;
                    call.input = input;
                    rewritten = true;
                }
                // A policy (a skill's `allowed-tools`) is the more specific
                // restriction: when it refuses the call too, its refusal is
                // the one reported, naming the hook's as well, so the worker
                // learns what actually restricts it. Checked on the input as
                // it stands; the call will not run, so no later rewrite can
                // change the outcome.
                HookDecision::Deny { reason } => {
                    if let Some((policy, refusal)) = self.policy_refusal(registered, &call.input) {
                        let both = format!("{refusal}; `{by}` also refused it: {reason}");
                        return deny(log, &call, &policy, both).await;
                    }
                    return deny(log, &call, &by, reason).await;
                }
                // Asking the user about a call a policy refuses anyway wastes
                // their answer and, in a run nobody attends, turns into a
                // denial that never names the policy. The policy refuses it
                // first. A later hook could in principle rewrite the input
                // into one the policy permits; asking would not run that
                // rewrite before the answer either, so refusing here loses
                // nothing the ask would have had.
                HookDecision::Ask { question } => {
                    if let Some((policy, refusal)) = self.policy_refusal(registered, &call.input) {
                        let both = format!("{refusal}; `{by}` would also have asked to run it");
                        return deny(log, &call, &policy, both).await;
                    }
                    return self
                        .ask(log, call, by, question, index + 1, rewritten)
                        .await;
                }
            }
        }
        if let Some((policy, reason)) = self.policy_refusal(registered, &call.input) {
            return deny(log, &call, &policy, reason).await;
        }
        let access = match registered.tool().access(&call.input) {
            Ok(a) => a,
            // Malformed input is the caller's mistake, not a policy's: it is
            // labeled apart so the worker is not told a permission refused it
            // (a worker that read "denied by access" asked for write access
            // instead of fixing its patch).
            Err(e @ crate::error::ToolError::InvalidInput { .. }) => {
                return deny(log, &call, INVALID_INPUT, e.to_string()).await;
            }
            Err(e) => return deny(log, &call, "access", e.to_string()).await,
        };
        Ok(Verdict::Run(ApprovedCall {
            call_id: call.call_id,
            tool: call.tool,
            input: call.input,
            access,
            rewritten,
        }))
    }

    /// The first policy that refuses `input` for `registered`: its name and
    /// reason.
    fn policy_refusal(
        &self,
        registered: &crate::registry::RegisteredTool,
        input: &serde_json::Value,
    ) -> Option<(String, String)> {
        if self.policies.is_empty() {
            return None;
        }
        let subject = registered.tool().permission_subject(input);
        self.policies.iter().find_map(|policy| {
            policy
                .check(registered.name(), subject.as_deref())
                .err()
                .map(|reason| (policy.name().to_owned(), reason))
        })
    }

    async fn ask(
        &self,
        log: &mut dyn ApprovalLog,
        call: PendingCall,
        asked_by: String,
        question: String,
        next_hook: usize,
        rewritten: bool,
    ) -> Result<Verdict, ApprovalError> {
        let deadline_ms = match self.ask_policy {
            AskPolicy::Interactive => None,
            AskPolicy::Unattended {
                on_ask: UnattendedAsk::Deny,
            } => {
                let asked = RecordedDecision::Asked {
                    question,
                    deadline_ms: None,
                };
                log.record(record(&call, &asked_by, asked)).await?;
                return deny(
                    log,
                    &call,
                    ASK_POLICY,
                    "unattended run: asks are denied".into(),
                )
                .await;
            }
            AskPolicy::Unattended {
                on_ask: UnattendedAsk::TimeoutThenDeny { after },
            } => {
                let after_ms = i64::try_from(after.as_millis()).unwrap_or(i64::MAX);
                Some((self.clock)().saturating_add(after_ms))
            }
        };
        let asked = RecordedDecision::Asked {
            question: question.clone(),
            deadline_ms,
        };
        log.record(record(&call, &asked_by, asked)).await?;
        let chain_digest = self.digest();
        let seal = self.seal(
            &call,
            &asked_by,
            &question,
            next_hook,
            rewritten,
            deadline_ms,
            &chain_digest,
        );
        Ok(Verdict::Ask(PausedCall {
            call,
            asked_by,
            question,
            next_hook,
            rewritten,
            deadline_ms,
            chain_digest,
            seal,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn seal(
        &self,
        call: &PendingCall,
        asked_by: &str,
        question: &str,
        next_hook: usize,
        rewritten: bool,
        deadline_ms: Option<i64>,
        chain_digest: &str,
    ) -> String {
        let mut h = blake3::Hasher::new_keyed(&self.seal_key);
        // The exact input bytes. serde_json serializes one `Value` to one byte
        // string, and a JSON round trip reproduces the `Value`, so a resumed call
        // seals the same as when it was paused.
        let input = serde_json::to_vec(&call.input).unwrap_or_default();
        for part in [
            call.call_id.as_bytes(),
            call.tool.as_bytes(),
            &input,
            call.worker.as_str().as_bytes(),
            call.task.as_str().as_bytes(),
            asked_by.as_bytes(),
            question.as_bytes(),
            &(next_hook as u64).to_le_bytes(),
            &[u8::from(rewritten)],
            format!("{deadline_ms:?}").as_bytes(),
            chain_digest.as_bytes(),
        ] {
            field(&mut h, part);
        }
        h.finalize().to_hex().to_string()
    }
}

const ASK_POLICY: &str = "ask-policy";

/// The `by` of a [`Verdict::Denied`] for a call whose input the tool could
/// not read (its `access` returned [`ToolError::InvalidInput`]). No policy
/// refused it; the call is malformed and the caller should fix and retry it.
pub const INVALID_INPUT: &str = "invalid-input";

async fn deny(
    log: &mut dyn ApprovalLog,
    call: &PendingCall,
    by: &str,
    reason: String,
) -> Result<Verdict, ApprovalError> {
    let decision = RecordedDecision::Denied {
        reason: reason.clone(),
    };
    log.record(record(call, by, decision)).await?;
    Ok(Verdict::Denied {
        by: by.to_owned(),
        reason,
    })
}

fn record(call: &PendingCall, by: &str, decision: RecordedDecision) -> ApprovalRecord {
    ApprovalRecord {
        call_id: call.call_id.clone(),
        tool: call.tool.clone(),
        decided_by: by.to_owned(),
        decision,
    }
}

/// Length-prefixed, so no two different field lists hash alike by concatenation.
fn field(h: &mut blake3::Hasher, bytes: &[u8]) {
    h.update(&(bytes.len() as u64).to_le_bytes());
    h.update(bytes);
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// A random key for this process, without adding a random-number dependency.
/// `RandomState` draws one 128-bit seed per thread from the operating
/// system's random source and derives each later instance from it by a
/// counter, so the four draws below fill 32 bytes but carry about 128 bits of
/// entropy, not 256. That is ample for a key no other process knows: the
/// seal only has to be infeasible to forge.
fn process_seal_key() -> [u8; 32] {
    use std::sync::OnceLock;
    static KEY: OnceLock<[u8; 32]> = OnceLock::new();
    *KEY.get_or_init(|| {
        let mut key = [0u8; 32];
        for (i, chunk) in key.chunks_mut(8).enumerate() {
            let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
            hasher.write_usize(i);
            chunk.copy_from_slice(&hasher.finish().to_le_bytes());
        }
        key
    })
}

/// An in-memory log, for tests and for callers that forward records elsewhere.
#[derive(Debug, Default)]
pub struct MemoryApprovalLog {
    pub records: Vec<ApprovalRecord>,
}

#[async_trait::async_trait]
impl ApprovalLog for MemoryApprovalLog {
    async fn record(&mut self, record: ApprovalRecord) -> Result<(), ApprovalError> {
        self.records.push(record);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{DiscoveredTools, Exposure, ToolRegistry};
    use crate::{CallPermit, Tool, ToolContext, ToolDefinition, ToolError, ToolOutput};
    use serde_json::{Value, json};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicI64, Ordering};

    /// A shell-like tool: subject is `cmd`, access writes `path` when present, and
    /// every executed input is recorded.
    #[derive(Default)]
    struct Shell {
        ran: Mutex<Vec<Value>>,
    }

    #[async_trait::async_trait]
    impl Tool for Shell {
        fn name(&self) -> &str {
            "shell"
        }
        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: "shell".into(),
                description: "run".into(),
                input_schema: json!({"type": "object"}),
            }
        }
        fn access(&self, input: &Value) -> Result<Access, ToolError> {
            match input["path"].as_str() {
                Some(p) => Access::none()
                    .write_path(p)
                    .map_err(|source| ToolError::Access {
                        tool: "shell".into(),
                        source,
                    }),
                None => Ok(Access::none()),
            }
        }
        fn permission_subject(&self, input: &Value) -> Option<String> {
            input["cmd"].as_str().map(str::to_owned)
        }
        async fn call(
            &self,
            input: Value,
            _ctx: &ToolContext,
            _permit: &CallPermit,
        ) -> Result<ToolOutput, ToolError> {
            self.ran.lock().expect("lock").push(input);
            Ok(ToolOutput::text("ran"))
        }
    }

    struct Fixed(&'static str, HookDecision);

    #[async_trait::async_trait]
    impl ApprovalHook for Fixed {
        fn name(&self) -> &str {
            self.0
        }
        async fn review(&self, _call: &PendingCall) -> HookDecision {
            self.1.clone()
        }
    }

    /// Denies any call whose input has `"danger": true`.
    struct DenyDanger;

    #[async_trait::async_trait]
    impl ApprovalHook for DenyDanger {
        fn name(&self) -> &str {
            "deny-danger"
        }
        async fn review(&self, call: &PendingCall) -> HookDecision {
            if call.input["danger"] == json!(true) {
                HookDecision::Deny {
                    reason: "dangerous".into(),
                }
            } else {
                HookDecision::Allow
            }
        }
    }

    /// Permits only subjects starting with `git `.
    struct GitOnly;

    impl CallPolicy for GitOnly {
        fn name(&self) -> &str {
            "skill:git"
        }
        fn fingerprint(&self) -> String {
            "shell(git:*)".into()
        }
        fn check(&self, _tool: &QualifiedName, subject: Option<&str>) -> Result<(), String> {
            match subject {
                Some(s) if s.starts_with("git ") => Ok(()),
                _ => Err("only git commands".into()),
            }
        }
    }

    struct RefusingLog;

    #[async_trait::async_trait]
    impl ApprovalLog for RefusingLog {
        async fn record(&mut self, _record: ApprovalRecord) -> Result<(), ApprovalError> {
            Err(ApprovalError::Log("disk full".into()))
        }
    }

    struct Fixture {
        registry: ToolRegistry,
        shell: Arc<Shell>,
        discovered: DiscoveredTools,
    }

    impl Fixture {
        fn new() -> Self {
            let shell = Arc::new(Shell::default());
            let mut registry = ToolRegistry::new();
            registry
                .register_builtin(shell.clone(), Exposure::Prefix)
                .expect("shell");
            Self {
                registry,
                shell,
                discovered: DiscoveredTools::new(),
            }
        }
        fn view(&self) -> ToolView<'_> {
            ToolView {
                registry: &self.registry,
                discovered: &self.discovered,
            }
        }
        fn ran(&self) -> Vec<Value> {
            self.shell.ran.lock().expect("lock").clone()
        }
        async fn execute(&self, approved: ApprovedCall) -> ToolOutput {
            let ctx = ToolContext {
                worker: "w1".into(),
                task: "t1".into(),
                workspace_root: std::env::temp_dir(),
                cancel: crate::CancellationToken::new(),
                progress: crate::ProgressSink::default(),
            };
            self.registry
                .execute(approved, &ctx)
                .await
                .expect("execute")
        }
    }

    fn call(input: Value) -> PendingCall {
        PendingCall {
            call_id: "c1".into(),
            tool: "shell".into(),
            input,
            worker: "w1".into(),
            task: "t1".into(),
        }
    }

    fn chain(hooks: Vec<Arc<dyn ApprovalHook>>) -> ApprovalChain {
        ApprovalChain::new(hooks)
    }

    fn ask_hook() -> Arc<dyn ApprovalHook> {
        Arc::new(Fixed(
            "ask",
            HookDecision::Ask {
                question: "run it?".into(),
            },
        ))
    }

    async fn paused(
        c: &ApprovalChain,
        f: &Fixture,
        input: Value,
        log: &mut MemoryApprovalLog,
    ) -> PausedCall {
        match c.review(f.view(), call(input), log).await.expect("review") {
            Verdict::Ask(p) => p,
            other => panic!("expected ask, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_rewrite_is_recorded_runs_rewritten_and_access_follows_the_final_input() {
        let f = Fixture::new();
        let c = chain(vec![Arc::new(Fixed(
            "sanitize",
            HookDecision::Rewrite {
                input: json!({"cmd": "ls", "path": "safe/out.txt"}),
                reason: "narrow".into(),
            },
        ))]);
        let mut log = MemoryApprovalLog::default();
        let verdict = c
            .review(
                f.view(),
                call(json!({"cmd": "ls -la /", "path": "src"})),
                &mut log,
            )
            .await
            .expect("review");
        let Verdict::Run(approved) = verdict else {
            panic!("expected run, got {verdict:?}");
        };
        assert!(approved.was_rewritten());
        assert!(
            approved
                .access()
                .writes_paths()
                .contains(std::path::Path::new("safe/out.txt"))
        );
        assert!(
            !approved
                .access()
                .writes_paths()
                .contains(std::path::Path::new("src"))
        );
        assert!(matches!(
            log.records[0].decision,
            RecordedDecision::Rewritten { ref original, .. } if original["cmd"] == "ls -la /"
        ));
        f.execute(approved).await;
        assert_eq!(f.ran(), vec![json!({"cmd": "ls", "path": "safe/out.txt"})]);
    }

    #[tokio::test]
    async fn an_empty_chain_records_that_the_call_ran_unreviewed() {
        let f = Fixture::new();
        let mut log = MemoryApprovalLog::default();
        let verdict = chain(vec![])
            .review(f.view(), call(json!({})), &mut log)
            .await
            .expect("review");
        assert!(matches!(verdict, Verdict::Run(_)));
        assert_eq!(log.records.len(), 1);
        assert_eq!(log.records[0].decision, RecordedDecision::AllowedNoHooks);
    }

    #[tokio::test]
    async fn a_log_that_refuses_the_write_withholds_the_approval() {
        let f = Fixture::new();
        let result = chain(vec![])
            .review(f.view(), call(json!({})), &mut RefusingLog)
            .await;
        assert!(matches!(result, Err(ApprovalError::Log(_))));
    }

    #[tokio::test]
    async fn a_later_hook_sees_an_earlier_rewrite_and_deny_ends_the_chain() {
        let f = Fixture::new();
        let c = chain(vec![
            Arc::new(Fixed(
                "mark",
                HookDecision::Rewrite {
                    input: json!({"danger": true}),
                    reason: "test".into(),
                },
            )),
            Arc::new(DenyDanger),
            Arc::new(Fixed("never", HookDecision::Allow)),
        ]);
        let mut log = MemoryApprovalLog::default();
        let verdict = c
            .review(f.view(), call(json!({})), &mut log)
            .await
            .expect("review");
        assert_eq!(
            verdict,
            Verdict::Denied {
                by: "deny-danger".into(),
                reason: "dangerous".into()
            }
        );
        assert_eq!(log.records.len(), 2, "the third hook never ran");
    }

    #[tokio::test]
    async fn a_policy_checks_the_final_input_after_rewrites() {
        let f = Fixture::new();
        let c = chain(vec![Arc::new(Fixed(
            "evil",
            HookDecision::Rewrite {
                input: json!({"cmd": "rm -rf x"}),
                reason: "r".into(),
            },
        ))])
        .with_policy(Arc::new(GitOnly));
        let mut log = MemoryApprovalLog::default();
        let verdict = c
            .review(f.view(), call(json!({"cmd": "git status"})), &mut log)
            .await
            .expect("review");
        assert!(matches!(verdict, Verdict::Denied { ref by, .. } if by == "skill:git"));
        assert!(f.ran().is_empty());
    }

    /// A hook asks about a call a policy refuses anyway: nobody is asked, and
    /// the refusal names the policy (the more specific restriction) and says
    /// the hook would have asked too. Before, the ask came first, a run with
    /// nobody attending denied it, and the worker never learned a skill
    /// restricted the call.
    #[tokio::test]
    async fn an_ask_on_a_call_a_policy_refuses_is_the_policys_refusal() {
        let f = Fixture::new();
        let c = chain(vec![ask_hook()]).with_policy(Arc::new(GitOnly));
        let mut log = MemoryApprovalLog::default();
        let verdict = c
            .review(f.view(), call(json!({"cmd": "wc -l x"})), &mut log)
            .await
            .expect("review");
        match verdict {
            Verdict::Denied { by, reason } => {
                assert_eq!(by, "skill:git");
                assert!(reason.contains("only git commands"), "{reason}");
                assert!(reason.contains("`ask` would also have asked"), "{reason}");
            }
            other => panic!("expected the policy's refusal, got {other:?}"),
        }
        assert!(
            !log.records
                .iter()
                .any(|r| matches!(r.decision, RecordedDecision::Asked { .. })),
            "nobody is asked about a call that cannot run: {:?}",
            log.records
        );
        // A call the policy permits is still asked about.
        let verdict = c
            .review(f.view(), call(json!({"cmd": "git status"})), &mut log)
            .await
            .expect("review");
        assert!(matches!(verdict, Verdict::Ask(_)), "{verdict:?}");
    }

    /// A hook denies a call a policy refuses too: the refusal names both, the
    /// policy first.
    #[tokio::test]
    async fn a_deny_on_a_call_a_policy_refuses_names_both() {
        let f = Fixture::new();
        let c = chain(vec![Arc::new(Fixed(
            "mode",
            HookDecision::Deny {
                reason: "not on the allowlist".into(),
            },
        ))])
        .with_policy(Arc::new(GitOnly));
        let mut log = MemoryApprovalLog::default();
        let verdict = c
            .review(f.view(), call(json!({"cmd": "wc -l x"})), &mut log)
            .await
            .expect("review");
        match verdict {
            Verdict::Denied { by, reason } => {
                assert_eq!(by, "skill:git");
                assert!(
                    reason.starts_with("only git commands")
                        && reason.contains("`mode` also refused it: not on the allowlist"),
                    "{reason}"
                );
            }
            other => panic!("{other:?}"),
        }
        // A deny the policy would permit stays the hook's own.
        let verdict = c
            .review(f.view(), call(json!({"cmd": "git push"})), &mut log)
            .await
            .expect("review");
        assert!(matches!(verdict, Verdict::Denied { ref by, .. } if by == "mode"));
    }

    /// A policy that refuses every call once closed.
    struct Closable(Arc<std::sync::atomic::AtomicBool>);

    impl CallPolicy for Closable {
        fn name(&self) -> &str {
            "closable"
        }
        fn fingerprint(&self) -> String {
            "closable".into()
        }
        fn check(&self, _tool: &QualifiedName, _subject: Option<&str>) -> Result<(), String> {
            if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                Err("closed".into())
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn a_recheck_refuses_an_approved_call_the_policy_narrowed_since() {
        let f = Fixture::new();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = chain(vec![]).with_policy(Arc::new(Closable(closed.clone())));
        let mut log = MemoryApprovalLog::default();
        let Verdict::Run(approved) = c
            .review(f.view(), call(json!({"cmd": "git status"})), &mut log)
            .await
            .expect("review")
        else {
            panic!("open policy approves");
        };
        assert_eq!(c.recheck(&f.registry, &approved), Ok(()));
        closed.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            c.recheck(&f.registry, &approved),
            Err(PolicyRefusal {
                by: "closable".into(),
                reason: "closed".into(),
            })
        );
        // No policy: nothing to recheck.
        assert_eq!(chain(vec![]).recheck(&f.registry, &approved), Ok(()));
    }

    #[tokio::test]
    async fn an_ask_pauses_survives_a_round_trip_and_a_user_allow_continues() {
        let f = Fixture::new();
        let c = chain(vec![ask_hook(), Arc::new(DenyDanger)]);
        let mut log = MemoryApprovalLog::default();
        let p = paused(&c, &f, json!({"cmd": "ls"}), &mut log).await;
        assert_eq!(p.question(), "run it?");
        let p: PausedCall =
            serde_json::from_str(&serde_json::to_string(&p).expect("ser")).expect("de");
        let verdict = c
            .answer(f.view(), p, UserAnswer::Allow, &mut log)
            .await
            .expect("answer");
        let Verdict::Run(approved) = verdict else {
            panic!("expected run");
        };
        f.execute(approved).await;
        assert_eq!(f.ran(), vec![json!({"cmd": "ls"})]);
        let decisions: Vec<&RecordedDecision> = log.records.iter().map(|r| &r.decision).collect();
        assert!(matches!(decisions[0], RecordedDecision::Asked { .. }));
        assert_eq!(decisions[1], &RecordedDecision::UserAllowed);
        assert_eq!(decisions[2], &RecordedDecision::Allowed);
    }

    #[tokio::test]
    async fn an_input_changed_after_the_ask_is_refused_and_never_runs() {
        // Edit the paused call's input to `rm -rf /`, then allow.
        let f = Fixture::new();
        let c = chain(vec![ask_hook()]);
        let mut log = MemoryApprovalLog::default();
        let p = paused(&c, &f, json!({"cmd": "ls"}), &mut log).await;
        let mut value = serde_json::to_value(&p).expect("value");
        value["call"]["input"]["cmd"] = json!("rm -rf /");
        let tampered: PausedCall = serde_json::from_value(value).expect("de");
        let result = c
            .answer(f.view(), tampered, UserAnswer::Allow, &mut log)
            .await;
        assert!(matches!(
            result,
            Err(ApprovalError::PausedCallAltered { .. })
        ));
        assert!(f.ran().is_empty());
        assert_eq!(log.records.len(), 1, "no user decision was recorded");
    }

    #[tokio::test]
    async fn a_resume_position_changed_after_the_ask_is_refused() {
        let f = Fixture::new();
        let c = chain(vec![ask_hook(), Arc::new(DenyDanger)]);
        let mut log = MemoryApprovalLog::default();
        let p = paused(&c, &f, json!({"danger": true}), &mut log).await;
        let mut value = serde_json::to_value(&p).expect("value");
        value["next_hook"] = json!(2); // skip DenyDanger
        let tampered: PausedCall = serde_json::from_value(value).expect("de");
        assert!(matches!(
            c.answer(f.view(), tampered, UserAnswer::Allow, &mut log)
                .await,
            Err(ApprovalError::PausedCallAltered { .. })
        ));
    }

    #[tokio::test]
    async fn a_seal_recomputed_without_the_key_is_refused() {
        let f = Fixture::new();
        let c = chain(vec![ask_hook()]);
        let other = chain(vec![ask_hook()]).with_seal_key([7; 32]);
        let mut log = MemoryApprovalLog::default();
        // A forger with a chain of the same shape but not this process's key.
        let forged = paused(&other, &f, json!({"cmd": "rm -rf /"}), &mut log).await;
        assert!(matches!(
            c.answer(f.view(), forged, UserAnswer::Allow, &mut log)
                .await,
            Err(ApprovalError::PausedCallAltered { .. })
        ));
    }

    #[tokio::test]
    async fn a_different_chain_cannot_answer_a_paused_call() {
        let f = Fixture::new();
        let c = chain(vec![ask_hook(), Arc::new(DenyDanger)]);
        let mut log = MemoryApprovalLog::default();
        let p = paused(&c, &f, json!({}), &mut log).await;
        let weaker = chain(vec![ask_hook()]);
        assert!(matches!(
            weaker
                .answer(f.view(), p, UserAnswer::Allow, &mut log)
                .await,
            Err(ApprovalError::ChainChanged { .. })
        ));
    }

    #[tokio::test]
    async fn a_user_deny_is_recorded_and_refuses() {
        let f = Fixture::new();
        let c = chain(vec![ask_hook()]);
        let mut log = MemoryApprovalLog::default();
        let p = paused(&c, &f, json!({}), &mut log).await;
        let verdict = c
            .answer(
                f.view(),
                p,
                UserAnswer::Deny {
                    reason: "not now".into(),
                },
                &mut log,
            )
            .await
            .expect("answer");
        assert!(matches!(verdict, Verdict::Denied { ref by, .. } if by == "user"));
        assert!(matches!(
            log.records.last().map(|r| &r.decision),
            Some(RecordedDecision::UserDenied { .. })
        ));
    }

    #[tokio::test]
    async fn an_ask_in_an_unattended_run_is_denied_and_logged() {
        let f = Fixture::new();
        let c = chain(vec![ask_hook()]).with_ask_policy(AskPolicy::Unattended {
            on_ask: UnattendedAsk::Deny,
        });
        let mut log = MemoryApprovalLog::default();
        let verdict = c
            .review(f.view(), call(json!({})), &mut log)
            .await
            .expect("review");
        assert!(matches!(verdict, Verdict::Denied { ref by, .. } if by == "ask-policy"));
        assert!(matches!(
            log.records[0].decision,
            RecordedDecision::Asked { .. }
        ));
        assert!(matches!(
            log.records[1].decision,
            RecordedDecision::Denied { .. }
        ));
        assert_eq!(log.records[1].decided_by, "ask-policy");
        assert!(f.ran().is_empty());
    }

    #[tokio::test]
    async fn an_unattended_ask_with_a_timeout_is_denied_after_the_deadline() {
        let f = Fixture::new();
        let now = Arc::new(AtomicI64::new(1_000));
        let clock = now.clone();
        let c = chain(vec![ask_hook()])
            .with_ask_policy(AskPolicy::Unattended {
                on_ask: UnattendedAsk::TimeoutThenDeny {
                    after: Duration::from_secs(60),
                },
            })
            .with_clock(move || clock.load(Ordering::SeqCst));
        let mut log = MemoryApprovalLog::default();
        let p = paused(&c, &f, json!({}), &mut log).await;
        assert_eq!(p.deadline_ms(), Some(61_000));

        // Before the deadline, expire hands it back.
        let Verdict::Ask(p) = c.expire(p, &mut log).await.expect("expire") else {
            panic!("not yet expired");
        };
        // After it, even a user allow is refused and the expiry is recorded.
        now.store(61_000, Ordering::SeqCst);
        let verdict = c
            .answer(f.view(), p, UserAnswer::Allow, &mut log)
            .await
            .expect("answer");
        assert!(matches!(verdict, Verdict::Denied { ref by, .. } if by == "ask-policy"));
        assert_eq!(
            log.records.last().map(|r| &r.decision),
            Some(&RecordedDecision::AskExpired)
        );
        assert!(f.ran().is_empty());
    }

    #[tokio::test]
    async fn unknown_and_undiscovered_tools_are_denied_and_logged() {
        let mut f = Fixture::new();
        let deferred = Arc::new(Shell::default());
        struct Renamed(Arc<Shell>);
        #[async_trait::async_trait]
        impl Tool for Renamed {
            fn name(&self) -> &str {
                "rare"
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "rare".into(),
                    description: "a rarely used tool".into(),
                    input_schema: json!({"type": "object"}),
                }
            }
            async fn call(
                &self,
                input: Value,
                ctx: &ToolContext,
                permit: &CallPermit,
            ) -> Result<ToolOutput, ToolError> {
                self.0.call(input, ctx, permit).await
            }
        }
        f.registry
            .register_builtin(Arc::new(Renamed(deferred)), Exposure::Deferred)
            .expect("rare");
        let c = chain(vec![]);
        let mut log = MemoryApprovalLog::default();
        let mut unknown = call(json!({}));
        unknown.tool = "missing".into();
        assert!(matches!(
            c.review(f.view(), unknown, &mut log).await.expect("review"),
            Verdict::Denied { ref by, .. } if by == "registry"
        ));
        let mut rare = call(json!({}));
        rare.tool = "rare".into();
        assert!(matches!(
            c.review(f.view(), rare.clone(), &mut log).await.expect("review"),
            Verdict::Denied { ref reason, .. } if reason.contains("not been discovered")
        ));
        f.discovered.discover(&f.registry, "rarely");
        let verdict = c.review(f.view(), rare, &mut log).await.expect("review");
        let Verdict::Run(approved) = verdict else {
            panic!("discovered tool must be approvable");
        };
        // Its access was not declared, so it is exclusive.
        assert!(approved.access().is_exclusive());
        assert_eq!(log.records.len(), 3);
    }

    #[tokio::test]
    async fn an_escaping_path_in_the_final_input_is_denied() {
        let f = Fixture::new();
        let mut log = MemoryApprovalLog::default();
        let verdict = chain(vec![])
            .review(f.view(), call(json!({"path": "../../etc"})), &mut log)
            .await
            .expect("review");
        assert!(matches!(verdict, Verdict::Denied { ref by, .. } if by == "access"));
    }

    /// Input the tool cannot read is denied as invalid input, never as an
    /// access or policy refusal.
    #[tokio::test]
    async fn malformed_input_is_denied_as_invalid_input_not_by_access() {
        struct Strict;
        #[async_trait::async_trait]
        impl Tool for Strict {
            fn name(&self) -> &str {
                "shell"
            }
            fn definition(&self) -> ToolDefinition {
                ToolDefinition {
                    name: "shell".into(),
                    description: "strict".into(),
                    input_schema: json!({"type": "object"}),
                }
            }
            fn access(&self, _input: &Value) -> Result<Access, ToolError> {
                Err(ToolError::InvalidInput {
                    tool: "shell".into(),
                    reason: "The first line of the patch must be '*** Begin Patch'".into(),
                })
            }
            async fn call(
                &self,
                _input: Value,
                _ctx: &ToolContext,
                _permit: &CallPermit,
            ) -> Result<ToolOutput, ToolError> {
                unreachable!("never approved")
            }
        }
        let mut registry = ToolRegistry::new();
        registry
            .register_builtin(Arc::new(Strict), Exposure::Prefix)
            .expect("strict");
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry: &registry,
            discovered: &discovered,
        };
        let mut log = MemoryApprovalLog::default();
        let verdict = chain(vec![])
            .review(view, call(json!({"patch": "oops"})), &mut log)
            .await
            .expect("review");
        let Verdict::Denied { by, reason } = verdict else {
            panic!("{verdict:?}");
        };
        assert_eq!(by, INVALID_INPUT);
        assert!(
            reason.starts_with("invalid input for tool `shell`"),
            "{reason}"
        );
    }
}
