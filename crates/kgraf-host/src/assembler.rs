//! Each attempt's worker environment, and each route's own. Moved from
//! `kgraf-cli`'s `orchestrate_cmd.rs` without a change in behavior; the one
//! addition is [`ResolverOverride`], which a host with its own ask surface (the
//! desktop shell's box) puts around the assembler.

use crate::error::{Error, Result, ResultExt};
use crate::host::Notice;
use crate::worker::ask::Attendance;
use crate::worker::assemble::{Assembled, Assembly, GraphParts, allow_store_path, assemble};
use crate::worker::config::WorkerConfig;
use crate::workspaces::{GitWorkspaces, now_nanos};
use kgraf_graph::GraphStore;
use kgraf_orchestrator::{
    AttemptEnv, AttemptEnvs, CloseEnv, FnProbe, MachineGate, ReadinessFailure, Route, SessionLine,
};
use kgraf_provider::{Ledger, Provider};
use kgraf_runtime::{AskResolver, CallMode, ContextConfig};
use kgraf_sandbox::{Workspace, WorkspaceManager, WorkspaceSpec};
use kgraf_session::approval_store::PersistedAllowStore;
use kgraf_tools::execpolicy::ExecPolicy;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One route's provider, as every environment on it is assembled with.
pub(crate) struct RouteParts {
    pub(crate) provider: Arc<dyn Provider>,
    /// The catalog vendor, route-qualified (part of the prefix identity, 0043).
    pub(crate) vendor: String,
    /// The catalog vendor alone (`Route::vendor`): two routes to one vendor
    /// share it.
    pub(crate) vendor_name: String,
    pub(crate) model: String,
}

/// Assembles each attempt's worker environment in the attempt's own worktree,
/// through the `assemble` that `kgraf worker` uses: the shell tool bound to
/// that worktree (its sandbox policy comes from it) with its approval hook in
/// the chain that reviews its calls, the configured MCP servers connected (a
/// stdio server without a `cwd` starts in a clean directory under the state
/// directory, never in the worktree) with every exchange attributed to the
/// worker in the ledger, and the graph tools. A delegate never asks the user:
/// shell commands and MCP calls (`McpApprovalHook`) that no persisted rule
/// allows are denied (`Attendance::Unattended`).
pub(crate) struct AttemptAssembler {
    pub(crate) root: PathBuf,
    pub(crate) state: PathBuf,
    pub(crate) graph_path: PathBuf,
    pub(crate) indexed: bool,
    pub(crate) ledger: Arc<Ledger>,
    pub(crate) config: WorkerConfig,
    pub(crate) max_output_tokens: u32,
    /// The run's `RunConfig::max_checkpoints` (decision 0096).
    pub(crate) max_checkpoints: u32,
    /// The run's reasoning setting (decision 0098), for every attempt's
    /// worker rounds and compaction summaries.
    pub(crate) reasoning: kgraf_provider::ReasoningLevels,
    pub(crate) read_denies: Vec<PathBuf>,
    pub(crate) workspaces: Arc<GitWorkspaces>,
    pub(crate) routes: BTreeMap<String, RouteParts>,
    /// Tools every environment of the run adds (`Assembly::extra_tools`): the
    /// route probes and every attempt get the same list.
    pub(crate) extra_tools: Vec<Arc<dyn kgraf_tools::Tool>>,
    /// Bound to the orchestrator once it exists, when the run registers
    /// session tools (decision 0067; `Assembly::session_line`).
    pub(crate) session_line: Option<SessionLine>,
    /// The run's own prefix registry (`Assembly::prefixes`).
    pub(crate) prefixes: Arc<kgraf_runtime::PrefixRegistry>,
    /// The host's shared execution-slot gate (decision 0067;
    /// `Assembly::machine_gate`): every attempt's `ShellTool` gates its
    /// commands through this, the same `MachineGate` `Host::admission` is
    /// bound to.
    pub(crate) machine_gate: Arc<MachineGate>,
    /// A Unix socket a Python Harbor shim listens on. Each attempt's own
    /// worktree opens its own connection and sends its own `hello` (see
    /// `assembly` below). The shim's task container has one filesystem, so a
    /// run with this set must keep contracts serialized (`--max-agents 1`);
    /// the shim brings the container to each new worktree at its `hello`.
    /// `None` for every other run.
    pub(crate) tbench_bridge_socket: Option<PathBuf>,
    /// The run's dependency refetcher (decision 0087;
    /// `Assembly::dependency_refetch`).
    pub(crate) dependency_refetch: Arc<dyn kgraf_tools::DependencyRefetch>,
    /// The project's skills, read once when the run is built
    /// (`Assembly::skills`), so the route probes and every attempt list the
    /// same catalog.
    pub(crate) skills: Option<Arc<kgraf_session::skills::SkillCatalog>>,
    /// Every attempt's skill session is registered here, the directory the
    /// run's `report_shortfall` reads a worker's restrictions from.
    pub(crate) skill_directory: Arc<kgraf_session::skills::SkillDirectory>,
    /// Every attempt's worker mirrors its policy refusals here, the
    /// directory the run's `report_shortfall` grounds a report in.
    pub(crate) refusal_directory: Arc<kgraf_runtime::RefusalDirectory>,
    /// The user's live permission control (decision 0094).
    pub(crate) permissions: Arc<kgraf_tools::PermissionControl>,
}

impl AttemptAssembler {
    pub(crate) fn assembly(
        &self,
        route: &str,
        ws: Workspace,
        worker: &str,
        task: &str,
    ) -> Result<Assembly> {
        let parts = self
            .routes
            .get(route)
            .ok_or_else(|| Error::Invalid(format!("route `{route}` is not configured")))?;
        let graph = if self.indexed {
            Some(GraphParts {
                // The run's own indexing pass is the writer that may rebuild;
                // an attempt only opens what it left.
                slices: GraphStore::open_existing_for_facts(&self.graph_path, &self.root).lift()?,
                tool: GraphStore::open_existing_for_facts(&self.graph_path, &self.root).lift()?,
            })
        } else {
            None
        };
        let (allow_store, _) = PersistedAllowStore::open(allow_store_path(&self.state), ws.root())
            .step("cannot open the project's allow rules")?;
        let project = kgraf_core::plain_path::canonicalize(&self.root)
            .with_step(|| format!("cannot resolve {}", self.root.display()))?
            .to_string_lossy()
            .into_owned();
        // This attempt's own worktree is known now, so it connects and sends
        // its own `hello` here, rather than at some earlier point that would
        // only know the base checkout.
        // `block_in_place` + `Handle::current().block_on` is the documented
        // way to run a short blocking-async call from inside an async task on
        // a multi-threaded runtime (`kgraf orchestrate`'s own runtime is
        // built with `new_multi_thread`) without blocking the whole runtime.
        let external_executor: Option<Arc<dyn kgraf_tools::external::ExternalExecutor>> =
            match &self.tbench_bridge_socket {
                Some(socket) => {
                    let ws_root = ws.root().to_path_buf();
                    let socket = socket.clone();
                    let executor = tokio::task::block_in_place(|| {
                        tokio::runtime::Handle::current().block_on(
                            kgraf_tbench_bridge::ProcessExecutor::connect(&socket, &ws_root),
                        )
                    })
                    .map_err(|e| {
                        Error::Invalid(format!(
                            "cannot connect to the tbench bridge socket {}: {e}",
                            socket.display()
                        ))
                    })?;
                    Some(Arc::new(executor))
                }
                None => None,
            };
        let path_case = kgraf_tools::detect_path_case(ws.root())
            .step("cannot probe the worktree's filesystem case behavior")?;
        // Decision 0094: one guard per attempt, so its snapshot, checks and
        // failure belong to this attempt alone.
        let protected_writes = kgraf_tools::ProtectedWrites::for_workspace(
            self.permissions.clone(),
            worker,
            &ws,
            self.state.clone(),
            settings_check(self.root.clone()),
        )
        .map_err(Error::Invalid)?;
        Ok(Assembly {
            ledger: self.ledger.clone(),
            provider: parts.provider.clone(),
            vendor: parts.vendor.clone(),
            model: parts.model.clone(),
            workspace: ws,
            checkout: self.root.clone(),
            graph,
            config: self.config.clone(),
            worker: worker.into(),
            task: task.into(),
            exec_policy: ExecPolicy::default_kgraf_policy(),
            allow_store,
            project,
            attendance: Attendance::Unattended,
            call_mode: CallMode::Complete,
            max_output_tokens: self.max_output_tokens,
            max_checkpoints: self.max_checkpoints,
            reasoning: self.reasoning,
            path_case,
            read_denies: self.read_denies.clone(),
            mcp_home: self.state.join("mcp-home"),
            extra_tools: self.extra_tools.clone(),
            session_line: self.session_line.clone(),
            prefixes: self.prefixes.clone(),
            machine_gate: Some(self.machine_gate.clone()),
            external_executor,
            dependency_refetch: Some(self.dependency_refetch.clone()),
            skills: self.skills.clone(),
            permissions: self.permissions.clone(),
            protected_writes: Some(Arc::new(protected_writes)),
            refusals: Some(self.refusal_directory.clone()),
        })
    }

    /// One attempt's environment, assembled in its live worktree, with the
    /// programs granted to its contract (decision 0093) allowed by its shell.
    async fn assembled(
        &self,
        route: &Route,
        workspace: &Path,
        worker: &kgraf_core::AgentId,
        task: &kgraf_core::TaskId,
        granted: &[String],
    ) -> std::result::Result<Assembled, String> {
        let ws = self
            .workspaces
            .get(workspace)
            .ok_or_else(|| format!("{} is not a live worktree of this run", workspace.display()))?;
        let mut assembly = self
            .assembly(&route.name, ws, worker.as_str(), task.as_str())
            .map_err(|e| format!("{e:#}"))?;
        assembly.config = assembly
            .config
            .with_granted_programs(granted.iter().cloned());
        let assembled = assemble(assembly).await.map_err(|e| format!("{e:#}"))?;
        if let Some(session) = &assembled.env.skills {
            self.skill_directory.register(session);
        }
        Ok(assembled)
    }
}

/// The check a worker's write to kGraf's settings must pass (decision
/// 0094): the parser the host loads each file with, so a file that would no
/// longer load (or that names a permission setting, which no project file may)
/// is restored.
pub(crate) fn settings_check(root: PathBuf) -> kgraf_tools::protected::SettingsCheck {
    Arc::new(move |name: &str, path: &Path| match name {
        "run.json" => crate::config::RunConfig::load(root.clone(), path)
            .map(|_| ())
            .map_err(|e| format!("{e:#}")),
        "worker.json" => WorkerConfig::load(path)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        other => Err(format!("`{other}` is not one of kGraf's settings files")),
    })
}

/// Closes an attempt's MCP connections after its worker ends.
struct AssembledCloser(Assembled);

#[async_trait::async_trait]
impl CloseEnv for AssembledCloser {
    async fn close(self: Box<Self>) {
        self.0.close().await
    }
}

#[async_trait::async_trait]
impl AttemptEnvs for AttemptAssembler {
    async fn build(
        &self,
        route: &Route,
        workspace: &Path,
        worker: &kgraf_core::AgentId,
        task: &kgraf_core::TaskId,
    ) -> std::result::Result<AttemptEnv, String> {
        self.build_granted(route, workspace, worker, task, &[])
            .await
    }

    async fn build_granted(
        &self,
        route: &Route,
        workspace: &Path,
        worker: &kgraf_core::AgentId,
        task: &kgraf_core::TaskId,
        granted: &[String],
    ) -> std::result::Result<AttemptEnv, String> {
        let assembled = self
            .assembled(route, workspace, worker, task, granted)
            .await?;
        Ok(AttemptEnv {
            env: assembled.env.clone(),
            closer: Some(Box::new(AssembledCloser(assembled))),
        })
    }
}

/// The assembler with the environment's ask resolver replaced: a host that
/// answers approval asks itself (the desktop shell's box) gets every ask the
/// chain raises, where the command's delegates deny what no rule allows. The
/// approval chain (the shell and MCP hooks) is the same one; only who answers
/// an ask changes.
pub(crate) struct ResolverOverride {
    pub(crate) inner: Arc<AttemptAssembler>,
    pub(crate) asks: Arc<dyn AskResolver>,
}

#[async_trait::async_trait]
impl AttemptEnvs for ResolverOverride {
    async fn build(
        &self,
        route: &Route,
        workspace: &Path,
        worker: &kgraf_core::AgentId,
        task: &kgraf_core::TaskId,
    ) -> std::result::Result<AttemptEnv, String> {
        self.build_granted(route, workspace, worker, task, &[])
            .await
    }

    async fn build_granted(
        &self,
        route: &Route,
        workspace: &Path,
        worker: &kgraf_core::AgentId,
        task: &kgraf_core::TaskId,
        granted: &[String],
    ) -> std::result::Result<AttemptEnv, String> {
        let mut assembled = self
            .inner
            .assembled(route, workspace, worker, task, granted)
            .await?;
        // `assemble` returns the only handle to a new environment, so this holds;
        // if it ever did not, the ask would silently go to the rule resolver
        // instead of the host, so it fails loudly.
        let Some(env) = Arc::get_mut(&mut assembled.env) else {
            assembled.close().await;
            return Err("the attempt environment is shared; its ask resolver cannot be set".into());
        };
        env.asks = Some(self.asks.clone());
        Ok(AttemptEnv {
            env: assembled.env.clone(),
            closer: Some(Box::new(AssembledCloser(assembled))),
        })
    }
}

/// Removes the route probe's worktree when dropped, unless it was already
/// removed on the normal path (which reports a failure to remove). On a tokio
/// runtime the removal (git) runs on a blocking thread, never on the async
/// worker that dropped the guard; with no runtime it runs here.
pub(crate) struct ProbeGuard {
    manager: Arc<WorkspaceManager>,
    ws: Option<Workspace>,
}

impl ProbeGuard {
    /// Create the probe worktree and its guard together, on the blocking
    /// thread: if the build is aborted while the worktree is being created,
    /// the guard is dropped with the task's discarded output and removes it.
    pub(crate) fn create(
        manager: Arc<WorkspaceManager>,
        name: String,
    ) -> tokio::task::JoinHandle<std::result::Result<ProbeGuard, kgraf_sandbox::WorkspaceError>>
    {
        tokio::task::spawn_blocking(move || {
            let ws = manager.create(&WorkspaceSpec {
                name: &name,
                base: "HEAD",
                branch: None,
            })?;
            Ok(ProbeGuard {
                manager,
                ws: Some(ws),
            })
        })
    }

    fn workspace(&self) -> Option<&Workspace> {
        self.ws.as_ref()
    }
}

impl Drop for ProbeGuard {
    fn drop(&mut self) {
        let Some(ws) = self.ws.take() else {
            return;
        };
        let manager = self.manager.clone();
        let remove = move || {
            if let Err(e) = manager.remove(ws) {
                eprintln!("could not remove a route probe worktree: {e}");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn_blocking(remove);
            }
            Err(_) => remove(),
        }
    }
}

/// Each route's own environment, assembled exactly as an attempt's is (so its
/// prefix, tool names and MCP instructions are an attempt's), in a throwaway
/// worktree removed afterwards. The orchestrator reads it for readiness, the
/// planner's tool list and sizing; workers run in their attempt's environment.
/// It also checks at startup that every configured MCP server connects and
/// lists its tools: an unreachable one stops the run here, loudly.
pub(crate) async fn route_envs(
    assembler: &AttemptAssembler,
    manager: &Arc<WorkspaceManager>,
    specs: &[String],
    context: &ContextConfig,
    run: &str,
    in_process: &std::collections::BTreeSet<String>,
    notices: &mut (dyn FnMut(Notice) + Send),
) -> Result<Vec<Route>> {
    // Removed on every exit, including this future being dropped (a build past
    // its time bound is aborted), even mid-creation: no probe worktree outlives
    // the build.
    let mut guard = ProbeGuard::create(manager.clone(), format!("probe-{}", now_nanos()))
        .await
        .lift()?
        .lift()?;
    let probe = guard
        .workspace()
        .cloned()
        .ok_or_else(|| Error::failed("the probe worktree was not created"))?;
    let mut routes = Vec::new();
    let mut failed = None;
    for spec in specs {
        let (route, model) = crate::config::split_route(spec)?;
        let name = format!("{route}:{model}");
        let built = async {
            let assembly =
                assembler.assembly(&name, probe.clone(), &format!("orchestrator:{run}"), run)?;
            assemble(assembly).await.lift()
        }
        .await;
        let assembled = match built {
            Ok(a) => a,
            Err(e) => {
                failed = Some(e.within(format!("route {name}")));
                break;
            }
        };
        for (server, report) in &assembled.registrations {
            notices(Notice::McpServer {
                route: name.clone(),
                server: server.clone(),
                report: report.clone(),
            });
        }
        let env = assembled.env.clone();
        assembled.close().await;
        let route_id = route.clone();
        let own = in_process.contains(&route);
        let probe_fn = FnProbe(Arc::new(move || {
            // An in-process provider has no catalog entry and no credential.
            if own {
                return Ok(());
            }
            let entry = kgraf_fleet::entry(&route_id)
                .map_err(|e| ReadinessFailure::MissingConfiguration(e.to_string()))?;
            kgraf_fleet::credential_from_env(entry)
                .map(|_| ())
                .map_err(|e| ReadinessFailure::MissingCredential(e.to_string()))
        }));
        let vendor = assembler
            .routes
            .get(&name)
            .map(|p| p.vendor_name.clone())
            .unwrap_or_else(|| route.clone());
        // Decision 0075: a route takes no credential when it is in-process
        // (no catalog entry to consult) or when its catalog entry declares it
        // (decision 0041's local endpoints: Ollama, llama.cpp, vLLM, LM
        // Studio). An entry lookup failure here is not this build's job to
        // report, readiness (`probe_fn`, above) already does that loudly;
        // routing treats an unresolvable entry as hosted, the conservative
        // default (it does not special-case cost, it only stops treating an
        // unpriced route as free).
        let local = own || kgraf_fleet::entry(&route).is_ok_and(|e| !e.requires_credential);
        routes.push(Route {
            name,
            vendor,
            env,
            context: context.clone(),
            readiness: Arc::new(probe_fn),
            local,
        });
    }
    let manager = manager.clone();
    let taken = guard.ws.take();
    drop(probe);
    if let Some(ws) = taken {
        tokio::task::spawn_blocking(move || manager.remove(ws))
            .await
            .lift()?
            .lift()?;
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(routes),
    }
}
