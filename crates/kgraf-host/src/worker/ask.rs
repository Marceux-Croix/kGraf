//! Who answers a worker's approval asks from the command line.
//!
//! The shell tool's hook asks about every command its policy classifies
//! `Prompt` (anything not allow-listed, including every `git` call). The answer
//! comes from, in order:
//!
//! 1. **The project's persisted allow rules** (`kgraf-session`'s
//!    `PersistedAllowStore`, `.kgraf/allow-rules.json` in the dispatching
//!    repository, never inside the worker's worktree). A shell rule is
//!    `Scope::Exact`: it matches only a byte-identical repeat of one argv, as
//!    the tool's canonical JSON subject, because no prefix of a command line
//!    reliably names what runs (the store refuses a prefix rule for `shell`).
//! 2. **The user at the terminal**, when stdin is one: `y` runs it once, `a`
//!    runs it and records a rule (an exact rule for a shell command; an
//!    any-call rule for an MCP tool, `mcp__<server>__<tool>`, which reports no
//!    subject), anything else denies it. An MCP tool can also be allowed
//!    ahead of time with `kgraf allow mcp__<server>__<tool>`
//!    ([`allow_mcp_tool`]), which is how an unattended run's delegates get one. An answer not
//!    given within the prompt timeout denies the call: an
//!    unanswered prompt must not hold a worker forever. The read stays blocked
//!    on its own thread; the command shuts its runtime down with
//!    `shutdown_background`, so that thread cannot hold the exit either.
//! 3. Otherwise **a denial**, saying why and how to allow it. A run nobody is
//!    watching never runs an unclassified command (0052's unattended rule).
//!
//! Why a resolver and not `PersistedAllowHook` in the chain: that hook asks
//! about every call no rule covers, of every tool, so the read-only file tools
//! would each need a rule. The store is consulted here, at the one point where a
//! hook has already decided a human is needed.

use kgraf_runtime::AskResolver;
use kgraf_session::approval_store::PersistedAllowStore;
use kgraf_tools::ToolRegistry;
use kgraf_tools::approval::{PausedCall, UserAnswer};
use std::io::{BufRead, IsTerminal, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Record a persisted rule allowing every call of the MCP tool `tool`
/// (`mcp__<server>__<tool>`) in `project`: `kgraf allow`. Only MCP tools:
/// a shell command is allowed per exact argv, at the terminal.
///
/// `configured` names the servers the worker configuration has. A tool of a
/// server not among them is still recorded (the configuration may be about to
/// gain it), and the returned warning says so, to be shown loudly.
pub fn allow_mcp_tool(
    state_dir: &std::path::Path,
    project: &str,
    tool: &str,
    note: &str,
    configured: &[String],
) -> anyhow::Result<Option<String>> {
    let server = mcp_server_of(tool)?;
    let mut store = open_store(state_dir)?;
    store.allow_any_call(
        project.to_owned(),
        tool.to_owned(),
        note.to_owned(),
        kgraf_session::approval_store::ConfirmAllowAnyCall,
    )?;
    Ok((!configured.iter().any(|c| c == server)).then(|| {
        format!(
            "warning: MCP server `{server}` is not in the worker configuration \
             (.kgraf/worker.json); the rule is recorded but allows nothing until it is"
        )
    }))
}

/// Remove the rules [`allow_mcp_tool`] recorded for `tool`; how many.
pub fn revoke_mcp_tool(
    state_dir: &std::path::Path,
    project: &str,
    tool: &str,
) -> anyhow::Result<usize> {
    mcp_server_of(tool)?;
    Ok(open_store(state_dir)?.revoke_any_call(project, tool)?)
}

fn open_store(state_dir: &std::path::Path) -> anyhow::Result<PersistedAllowStore> {
    // The store refuses a path inside a worktree; worktrees live here.
    let worktrees = state_dir.join("worktrees");
    std::fs::create_dir_all(&worktrees)?;
    let (store, _) =
        PersistedAllowStore::open(super::assemble::allow_store_path(state_dir), &worktrees)?;
    Ok(store)
}

/// The server of an MCP tool name `mcp__<server>__<tool>`; both parts ASCII
/// letters, digits, `_` and `-`, as the registry names tools.
fn mcp_server_of(tool: &str) -> anyhow::Result<&str> {
    let parts = tool
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"));
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    match parts {
        Some((server, name)) if ok(server) && ok(name) => Ok(server),
        _ => anyhow::bail!(
            "`{tool}` is not an MCP tool name (mcp__<server>__<tool>, ASCII letters, digits, \
             `_` and `-`); shell commands are allowed one exact command at a time, by \
             answering `a` at a terminal"
        ),
    }
}

/// How long a terminal prompt waits before it denies the call.
pub const DEFAULT_PROMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// How an ask reaches a person, if one is there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attendance {
    /// Prompt on the terminal; no answer within `timeout` denies.
    Terminal { timeout: Duration },
    /// Nobody to ask: deny what no rule covers.
    Unattended,
}

impl Attendance {
    /// `Terminal` when both stdin and stderr are terminals.
    pub fn detect(timeout: Duration) -> Self {
        if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
            Self::Terminal { timeout }
        } else {
            Self::Unattended
        }
    }
}

/// A person's answer to one prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Once,
    Always,
    No,
}

/// Asks the question (`question`, `tool`) and blocks for the answer. The
/// default reads the terminal; tests supply their own.
pub type Prompter = Arc<dyn Fn(&str, &str) -> std::io::Result<Choice> + Send + Sync>;

/// Answers asks from the persisted rules, then the terminal, then denies.
pub struct RuleOrPromptResolver {
    store: Arc<Mutex<PersistedAllowStore>>,
    project: String,
    registry: Arc<ToolRegistry>,
    attendance: Attendance,
    prompter: Prompter,
}

impl RuleOrPromptResolver {
    pub fn new(
        store: PersistedAllowStore,
        project: impl Into<String>,
        registry: Arc<ToolRegistry>,
        attendance: Attendance,
    ) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            project: project.into(),
            registry,
            attendance,
            prompter: Arc::new(prompt),
        }
    }

    /// Ask through `prompter` instead of the terminal.
    pub fn with_prompter(mut self, prompter: Prompter) -> Self {
        self.prompter = prompter;
        self
    }

    fn subject(&self, paused: &PausedCall) -> Option<String> {
        let call = paused.call();
        self.registry
            .get(&call.tool)?
            .tool()
            .permission_subject(&call.input)
    }
}

fn prompt(question: &str, tool: &str) -> std::io::Result<Choice> {
    let mut err = std::io::stderr().lock();
    // For an MCP tool, `a` records a rule for every call to the tool; say so,
    // with the same words as the orchestrator box's button.
    let always = if tool.starts_with("mcp__") {
        format!("[a]llow every call to {tool}")
    } else {
        "[a]lways".to_owned()
    };
    write!(
        err,
        "\n[approval] {tool}: {question} [y]es once / {always} / [N]o: "
    )?;
    err.flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Choice::Once,
        "a" | "always" => Choice::Always,
        _ => Choice::No,
    })
}

#[async_trait::async_trait]
impl AskResolver for RuleOrPromptResolver {
    async fn answer(&self, paused: &PausedCall) -> UserAnswer {
        let tool = paused.call().tool.clone();
        let subject = self.subject(paused);
        let covered = {
            let store = self.store.lock().unwrap_or_else(|p| p.into_inner());
            store
                .matching_rule(&self.project, &tool, subject.as_deref())
                .map(|rule| rule.note.clone())
        };
        if let Some(note) = covered {
            eprintln!("[approval] {tool}: allowed by a persisted rule ({note})");
            return UserAnswer::Allow;
        }
        let Attendance::Terminal { timeout } = self.attendance else {
            let how = if tool.starts_with("mcp__") {
                format!(
                    "record one with `kgraf allow {tool}`, or answer `a` when run attached \
                     to a terminal"
                )
            } else if subject.is_some() {
                "run attached to a terminal and answer `a` to record one for this exact call"
                    .to_owned()
            } else {
                "no rule can name this call; it needs an answer at a terminal".to_owned()
            };
            return UserAnswer::Deny {
                reason: format!(
                    "{} Nobody is attached to answer, and no persisted allow rule for this \
                     project covers it; {how}.",
                    paused.question()
                ),
            };
        };
        let question = paused.question().to_owned();
        let asked = tool.clone();
        let prompter = self.prompter.clone();
        // Reading stdin blocks; keep it off the executor, and stop waiting at
        // the timeout (the blocked read is left behind; see the module docs).
        let read = tokio::task::spawn_blocking(move || prompter(&question, &asked));
        let Ok(choice) = tokio::time::timeout(timeout, read).await else {
            eprintln!("\n[approval] no answer within {timeout:?}; denied");
            return UserAnswer::Deny {
                reason: format!("no answer at the terminal within {timeout:?}"),
            };
        };
        match choice {
            Ok(Ok(Choice::Once)) => UserAnswer::Allow,
            Ok(Ok(Choice::Always)) if subject.is_none() && tool.starts_with("mcp__") => {
                // An MCP tool reports no subject: the rule covers the tool.
                let recorded = {
                    let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                    store
                        .allow_any_call(
                            self.project.clone(),
                            tool.clone(),
                            "allowed at the terminal",
                            kgraf_session::approval_store::ConfirmAllowAnyCall,
                        )
                        .map(|_| ())
                };
                if let Err(e) = recorded {
                    eprintln!("[approval] the rule could not be saved: {e}");
                }
                UserAnswer::Allow
            }
            Ok(Ok(Choice::Always)) => {
                let Some(subject) = subject else {
                    eprintln!(
                        "[approval] {tool} reports no exact subject for this call, so no rule \
                         can name it; allowed once"
                    );
                    return UserAnswer::Allow;
                };
                let recorded = {
                    let mut store = self.store.lock().unwrap_or_else(|p| p.into_inner());
                    store
                        .allow_exact(
                            self.project.clone(),
                            tool.clone(),
                            subject,
                            "allowed at the terminal by `kgraf worker`",
                        )
                        .map(|_| ())
                };
                if let Err(e) = recorded {
                    // The call is still allowed: the user said yes. The rule
                    // was not saved, and that is said, not hidden.
                    eprintln!("[approval] the rule could not be saved: {e}");
                }
                UserAnswer::Allow
            }
            Ok(Ok(Choice::No)) => UserAnswer::Deny {
                reason: "denied at the terminal".into(),
            },
            Ok(Err(e)) => UserAnswer::Deny {
                reason: format!("the terminal could not be read: {e}"),
            },
            Err(e) => UserAnswer::Deny {
                reason: format!("the prompt task failed: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kgraf_tools::approval::{ApprovalChain, MemoryApprovalLog, PendingCall, Verdict};
    use kgraf_tools::execpolicy::ExecPolicy;
    use kgraf_tools::{DiscoveredTools, Exposure, ShellTool, ToolView};

    /// `kgraf allow` refuses a malformed name, warns for
    /// a server the configuration lacks, and `--revoke` removes the rule.
    #[test]
    fn allow_validates_warns_and_revokes() {
        let dir = tempfile::tempdir().expect("dir");
        let state = dir.path().join(".kgraf");
        let configured = vec!["fs".to_owned()];
        for bad in [
            "shell",
            "mcp__fs",
            "mcp____x",
            "mcp__f s__read",
            "mcp__fs__re;ad",
        ] {
            assert!(
                allow_mcp_tool(&state, "p", bad, "n", &configured).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            allow_mcp_tool(&state, "p", "mcp__fs__read", "n", &configured).expect("ok"),
            None
        );
        let warned = allow_mcp_tool(&state, "p", "mcp__other__x", "n", &configured)
            .expect("recorded")
            .expect("a warning");
        assert!(
            warned.contains("`other` is not in the worker configuration"),
            "{warned}"
        );
        let store = open_store(&state).expect("store");
        assert!(store.matching_rule("p", "mcp__fs__read", None).is_some());
        assert_eq!(
            revoke_mcp_tool(&state, "p", "mcp__fs__read").expect("revoke"),
            1
        );
        assert_eq!(
            revoke_mcp_tool(&state, "p", "mcp__fs__read").expect("again"),
            0
        );
        let store = open_store(&state).expect("store");
        assert!(store.matching_rule("p", "mcp__fs__read", None).is_none());
        assert!(store.matching_rule("p", "mcp__other__x", None).is_some());
    }

    /// A paused shell call, as the hook raises it for an unlisted command.
    async fn paused(registry: &ToolRegistry, chain: &ApprovalChain) -> PausedCall {
        let discovered = DiscoveredTools::new();
        let view = ToolView {
            registry,
            discovered: &discovered,
        };
        let verdict = chain
            .review(
                view,
                PendingCall {
                    call_id: "c".into(),
                    tool: "shell".into(),
                    input: serde_json::json!({"command": ["touch", "x"]}),
                    worker: "w".into(),
                    task: "t".into(),
                },
                &mut MemoryApprovalLog::default(),
            )
            .await
            .expect("review");
        match verdict {
            Verdict::Ask(paused) => paused,
            other => panic!("expected an ask, got {other:?}"),
        }
    }

    /// The terminal prompt had no timeout, so an unanswered
    /// ask held the worker forever. A prompter that never answers in time is
    /// now a denial at the timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unanswered_prompt_denies_at_the_timeout() {
        let (shell, hook) = ShellTool::new(ExecPolicy::default_kgraf_policy());
        let mut registry = ToolRegistry::new();
        registry
            .register_builtin(Arc::new(shell), Exposure::Prefix)
            .expect("register");
        let registry = Arc::new(registry);
        let chain = ApprovalChain::new(vec![Arc::new(hook)]);
        let paused = paused(&registry, &chain).await;
        let dir = tempfile::tempdir().expect("dir");
        let elsewhere = tempfile::tempdir().expect("elsewhere");
        let (store, _) = PersistedAllowStore::open(dir.path().join("rules.json"), elsewhere.path())
            .expect("store");
        let resolver = RuleOrPromptResolver::new(
            store,
            "p",
            registry,
            Attendance::Terminal {
                timeout: Duration::from_millis(100),
            },
        )
        .with_prompter(Arc::new(|_, _| {
            std::thread::sleep(Duration::from_millis(1_500));
            Ok(Choice::Once)
        }));
        let started = std::time::Instant::now();
        let answer = resolver.answer(&paused).await;
        assert!(
            started.elapsed() < Duration::from_millis(1_000),
            "{:?}",
            started.elapsed()
        );
        match answer {
            UserAnswer::Deny { reason } => assert!(reason.contains("no answer"), "{reason}"),
            UserAnswer::Allow => panic!("an unanswered prompt must deny"),
        }
    }
}
