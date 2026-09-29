//! Quality gates: the project's validation commands, discovered or configured,
//! run confined in a worktree of the integration commit they pin. Moved from
//! `kgraf-cli`'s `orchestrate_cmd.rs` without a change in behavior.

use crate::error::{Result, ResultExt};
use crate::workspaces::now_nanos;
use kgraf_orchestrator::{GateRequest, GateRunner, GateVerdict, UntrustedText};
use kgraf_sandbox::{
    MergeBack, Network, Redirect, SandboxPolicy, Workspace, WorkspaceManager, WorkspaceSpec,
    sandboxed_command,
};
// The process lifecycle (WNOWAIT, group kill until the hold is free, then
// reap) lives in the sandbox crate so the contract-check runner shares it.
#[cfg(unix)]
use kgraf_sandbox::lifecycle::{GROUP_EXIT_GRACE, exited_unreaped, kill_group_until_hold_free};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// The project's validation commands: the configured ones, or those its build
/// files declare. The runner is discovered, never invented (0019): a project
/// with none gets no gate, and the orchestrator records that.
pub fn discover_validation_commands(root: &Path, configured: &[String]) -> Result<Vec<String>> {
    if !configured.is_empty() {
        return Ok(configured.to_vec());
    }
    let mut found = Vec::new();
    if root.join("Cargo.toml").is_file() {
        found.push("cargo test --workspace".to_owned());
    }
    let package = root.join("package.json");
    if package.is_file() {
        let text = std::fs::read_to_string(&package)
            .with_step(|| format!("cannot read {}", package.display()))?;
        let json: serde_json::Value = serde_json::from_str(&text)
            .with_settings(|| format!("{} is not valid JSON", package.display()))?;
        if json["scripts"]["test"].is_string() {
            found.push("npm test".to_owned());
        }
    }
    if root.join("pyproject.toml").is_file() {
        found.push("python -m pytest".to_owned());
    }
    for name in ["Makefile", "makefile", "GNUmakefile"] {
        let path = root.join(name);
        if path.is_file() {
            let text = std::fs::read_to_string(&path)
                .with_step(|| format!("cannot read {}", path.display()))?;
            if text.lines().any(|l| l.starts_with("test:")) {
                found.push("make test".to_owned());
            }
            break;
        }
    }
    Ok(found)
}

/// Output a gate keeps from its commands, from the end (where failures are).
pub(crate) const GATE_OUTPUT_CAP: usize = 16 * 1024;

/// Runs the gate's commands in a sandbox, in a fresh worktree of the integration
/// commit the gate pinned, each with a timeout; the worktree is removed after.
pub(crate) struct CommandGateRunner {
    pub(crate) commands: Vec<String>,
    pub(crate) manager: Arc<WorkspaceManager>,
    pub(crate) merge_back: Arc<MergeBack>,
    pub(crate) read_denies: Vec<PathBuf>,
    pub(crate) timeout: Duration,
    pub(crate) output_cap: usize,
}

/// The last `cap` bytes of `text`, on a character boundary, marked when cut.
/// The command's log, read by the host. The log lies in the worktree's
/// scratch, which the sandboxed command can write, so it could be replaced by
/// a link (`gate-1.log -> ~/.ssh/id_ed25519`) or a hard link to a file the
/// sandbox denies it, and the host would read that file into the verdict text
/// (decision 0083). Opened no-follow, non-blocking, and read only when it is
/// a regular file with one link; anything else reads as a note saying so.
fn read_log(log: &Path) -> String {
    use std::io::Read;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let refused = || "[the command's log was replaced by a link or a special file; not read]";
    let Ok(file) = opts.open(log) else {
        return match log.symlink_metadata() {
            Ok(_) => refused().to_owned(),
            Err(_) => String::new(),
        };
    };
    let Ok(meta) = file.metadata() else {
        return refused().to_owned();
    };
    #[cfg(unix)]
    let linked = {
        use std::os::unix::fs::MetadataExt;
        meta.nlink() > 1
    };
    #[cfg(not(unix))]
    let linked = false;
    if !meta.is_file() || linked {
        return refused().to_owned();
    }
    let mut bytes = Vec::new();
    let _ = (&file).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn tail(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut start = text.len() - cap;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[{} bytes cut]\n{}", start, &text[start..])
}

/// Run one command confined to `ws`, output to a log in its scratch directory,
/// killed after `timeout`. Returns (success, output tail).
///
/// When the command's shell exits (or times out), its whole process group is
/// killed before the shell is reaped: a background process the command left
/// (`cmd &`) must not keep writing the worktree while the caller inspects or
/// cleans it. A process that left the group with its
/// own `setsid` is not reached by this; the caller's hold check
/// (`WorkspaceManager::quiesce`) finds it.
///
/// Stated rather than hidden: such an escaped descendant is **not killed**.
/// It runs on inside the same sandbox, with write access to the worktree and
/// its scratch only, until it exits by itself or `RLIMIT_CPU` stops it (the
/// same mechanism the contract-check runner uses,
/// `kgraf_sandbox::lifecycle::limit_cpu_seconds`, at
/// `lifecycle::cpu_rlimit_seconds(timeout)`: twice the gate's own timeout, at
/// least one second, so a normal multi-threaded build has headroom but a
/// spinning escapee is eventually stopped). It can race the worktree's
/// removal: a write after the removal lands in a directory the next startup
/// cleanup finds.
///
/// `checkout` is the checkout root the worktree was made from (the
/// directory holding `.kgraf`, `WorkspaceManager::repo`): cargo in the
/// worktree reads the checkout's `Cargo.toml` and `.cargo/config.toml` above
/// it, and those reads stop there (Linux; `with_ancestor_reads`). A cargo
/// file there that the sandbox refused is named, with its cause, after a
/// failing command's output.
pub(crate) fn run_confined(
    ws: &Workspace,
    checkout: &Path,
    denies: &[PathBuf],
    command: &str,
    timeout: Duration,
    cap: usize,
) -> std::result::Result<(bool, String), String> {
    let mut policy =
        SandboxPolicy::for_workspace(ws, Network::Deny).with_ancestor_reads(checkout.to_path_buf());
    for d in denies {
        policy = policy.deny_read(d.clone());
    }
    let log = ws.scratch().join(format!("gate-{}.log", now_nanos()));
    let mut cmd = sandboxed_command(&policy, "sh", ws.root())
        .map_err(|e| format!("cannot sandbox `{command}`: {e}"))?;
    let refused = refused_note(cmd.refused_reads());
    // The log path goes through the environment, never through the shell's
    // parser, so a path with spaces or quotes cannot change the command.
    cmd.args(["-c", &format!("exec >\"$KGRAF_GATE_LOG\" 2>&1\n{command}")])
        .env("KGRAF_GATE_LOG", &log)
        .env("CARGO_TARGET_DIR", ws.scratch().join("target"))
        // Python bytecode goes to scratch, outside the worktree, as cargo's build
        // output does: an import in the check writes no `__pycache__` into the
        // tree (and none that the worker's run left is read: it was removed).
        .env("PYTHONPYCACHEPREFIX", ws.scratch().join("pycache"))
        .stdin(Redirect::Null)
        .stdout(Redirect::Null)
        .stderr(Redirect::Null);
    // Bounds an escaped descendant's CPU (see the module doc comment); a
    // well-behaved command is already stopped at `timeout` by the group kill
    // below.
    #[cfg(unix)]
    // SAFETY: forwarded from `limit_cpu_seconds`'s own contract; the closure
    // it registers only calls `setrlimit` and allocates nothing.
    unsafe {
        kgraf_sandbox::lifecycle::limit_cpu_seconds(
            cmd.command_mut(),
            kgraf_sandbox::lifecycle::cpu_rlimit_seconds(timeout),
        );
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("cannot run `{command}`: {e}"))?;
    let started = std::time::Instant::now();
    let status = loop {
        #[cfg(unix)]
        {
            match exited_unreaped(child.id()) {
                Ok(true) => kill_group_until_hold_free(child.id(), ws.hold_file()),
                Ok(false) => {}
                Err(e) => return Err(format!("waiting for `{command}`: {e}")),
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if started.elapsed() >= timeout => {
                // The sandboxed child calls setsid() before exec, so it leads its
                // own process group: killing the group kills everything the
                // command started, not only the shell.
                #[cfg(unix)]
                // SAFETY: killpg only sends a signal to the child's own group.
                unsafe {
                    libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("waiting for `{command}`: {e}")),
        }
    };
    // SIGKILL is delivered asynchronously: wait (bounded) until no member of
    // the group is left, so the caller's hold check sees the group gone.
    #[cfg(unix)]
    {
        let deadline = std::time::Instant::now() + GROUP_EXIT_GRACE;
        // SAFETY: signal 0 only probes whether the group has members.
        while unsafe { libc::killpg(child.id() as libc::pid_t, 0) } == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    let output = read_log(&log);
    let _ = std::fs::remove_file(&log);
    let mut text = tail(&output, cap);
    match status {
        Some(s) => {
            text = format!("$ {command}\n{text}\n[{s}]");
            if !s.success() {
                text.push_str(&refused);
            }
            Ok((s.success(), text))
        }
        None => Ok((
            false,
            format!(
                "$ {command}\n{text}\n[killed after {}s]{refused}",
                timeout.as_secs()
            ),
        )),
    }
}

/// The sandbox's refused ancestor reads as lines to append to a failing
/// command's output (cargo's "Permission denied" alone does not
/// say why), or nothing.
pub(crate) fn refused_note(refused: &[String]) -> String {
    refused.iter().map(|r| format!("\n[sandbox] {r}")).collect()
}

#[async_trait::async_trait]
impl GateRunner for CommandGateRunner {
    fn commands(&self) -> Vec<String> {
        self.commands.clone()
    }

    async fn run(&self, request: GateRequest) -> std::result::Result<GateVerdict, String> {
        let manager = self.manager.clone();
        let merge_back = self.merge_back.clone();
        let denies = self.read_denies.clone();
        let timeout = self.timeout;
        let cap = self.output_cap;
        tokio::task::spawn_blocking(move || {
            // The pinned integration commit; never the live trunk (0018).
            let base = match &request.pinned {
                Some(c) => c.sha.clone(),
                None => merge_back.integration_tip().map_err(|e| e.to_string())?,
            };
            let name = format!("gate-{}", now_nanos());
            let ws = manager
                .create(&WorkspaceSpec {
                    name: &name,
                    base: &base,
                    branch: None,
                })
                .map_err(|e| format!("cannot create the gate worktree at {base}: {e}"))?;
            let mut passed = true;
            let mut output = String::new();
            for command in &request.commands {
                match run_confined(&ws, manager.repo(), &denies, command, timeout, cap) {
                    Ok((ok, text)) => {
                        output.push_str(&text);
                        output.push('\n');
                        if !ok {
                            passed = false;
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = manager.remove(ws);
                        return Err(e);
                    }
                }
            }
            manager
                .remove(ws)
                .map_err(|e| format!("cannot remove the gate worktree: {e}"))?;
            Ok(GateVerdict {
                passed,
                by: format!("{} at {base}", request.commands.join(" && ")),
                output: UntrustedText::new(tail(&output, cap)),
            })
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::read_log;

    /// Decision 0083: a gate command replaces its own log with a link or a
    /// hard link to a file outside; the host reads neither into the verdict.
    #[test]
    fn a_log_replaced_by_a_link_or_a_hard_link_is_never_read() {
        let dir = tempfile::tempdir().expect("dir");
        let scratch = dir.path().join("scratch");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&scratch).expect("mkdir");
        std::fs::create_dir_all(&outside).expect("mkdir");
        std::fs::write(outside.join("secret"), "SECRET-KEY\n").expect("write");

        let log = scratch.join("gate-1.log");
        std::os::unix::fs::symlink(outside.join("secret"), &log).expect("link");
        let text = read_log(&log);
        assert!(!text.contains("SECRET"), "{text}");
        assert!(text.contains("not read"), "{text}");

        let hard = scratch.join("gate-2.log");
        std::fs::hard_link(outside.join("secret"), &hard).expect("link");
        assert!(!read_log(&hard).contains("SECRET"));

        let plain = scratch.join("gate-3.log");
        std::fs::write(&plain, "ok\n").expect("write");
        assert_eq!(read_log(&plain), "ok\n");
        assert_eq!(read_log(&scratch.join("absent.log")), "");
    }
}
