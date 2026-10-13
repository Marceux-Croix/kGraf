//! The processes one attempt's commands start, found and ended when the
//! orchestrator is done with the attempt ([`AttemptProcesses`]).
//!
//! - **Host sandbox** ([`SandboxProcesses`]): every command logs its session
//!   beside the worktree's hold file, so what a command left running (a
//!   server started in the background) is found by that log and ended
//!   ([`kgraf_sandbox::end_worktree_processes`]). The tree is the whole
//!   delivery, so this runs as soon as the worker finishes.
//! - **Task container** ([`MarkedExecutor`]): every command the attempt
//!   routes to the external executor runs under `env KGRAF_ATTEMPT=<worker>`,
//!   which every process it starts inherits, so the attempt's processes are
//!   the container's processes that carry the mark (read from
//!   `/proc/<pid>/environ`). The container's state is part of what the task
//!   judges (a server the task asked for must still run), so only the
//!   attempts that are not accepted are ended.
//!
//! Either way an ending that found something is reported in the run's
//! notices and trace; one that failed is reported as a failure, never
//! skipped.

use kgraf_orchestrator::AttemptProcesses;
use kgraf_tools::external::{ExternalExecutor, ExternalOutcome};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The environment variable every routed command of an attempt carries.
pub(crate) const ATTEMPT_MARK_VAR: &str = "KGRAF_ATTEMPT";

/// How long the container is given to list and end the marked processes.
const END_TIMEOUT: Duration = Duration::from_secs(30);

/// A worktree's commands in the host sandbox.
pub(crate) struct SandboxProcesses {
    /// The worktree's hold file (`Workspace::hold_file`).
    pub(crate) hold: PathBuf,
}

#[async_trait::async_trait]
impl AttemptProcesses for SandboxProcesses {
    fn delivered(&self) -> bool {
        false
    }

    async fn end(&self) -> Result<Vec<String>, String> {
        let hold = self.hold.clone();
        let ended =
            tokio::task::spawn_blocking(move || kgraf_sandbox::end_worktree_processes(&hold))
                .await
                .map_err(|e| format!("the ending task failed: {e}"))?
                .map_err(|e| e.to_string())?;
        if !ended.survivors.is_empty() {
            return Err(format!(
                "session(s) {:?} still have a running process after `SIGKILL` (ended {:?})",
                ended.survivors, ended.processes
            ));
        }
        let mut lines = Vec::new();
        if !ended.processes.is_empty() {
            lines.push(format!(
                "sandbox: ended process(es) {:?} of command session(s) {:?}",
                ended.processes, ended.sessions
            ));
        }
        if ended.foreign > 0 {
            lines.push(format!(
                "{} command session(s) were started in another PID namespace and could not be \
                 checked from here",
                ended.foreign
            ));
        }
        Ok(lines)
    }
}

/// An attempt's view of the external executor: every command it runs is
/// marked with the attempt, so the processes it leaves can be found again.
pub(crate) struct MarkedExecutor {
    inner: Arc<dyn ExternalExecutor>,
    /// `KGRAF_ATTEMPT=<worker>`.
    mark: String,
}

impl MarkedExecutor {
    pub(crate) fn new(inner: Arc<dyn ExternalExecutor>, worker: &str) -> Self {
        // Only characters a shell and `/proc/<pid>/environ` keep as they are.
        let id: String = worker
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        Self {
            inner,
            mark: format!("{ATTEMPT_MARK_VAR}={id}"),
        }
    }

    /// `cmd` run under the attempt's mark: `env` sets the variable and runs
    /// `cmd` as given (argv, no shell), so the command's own exit code and
    /// output are what the worker sees.
    fn marked(&self, cmd: &[String]) -> Vec<String> {
        let mut out = Vec::with_capacity(cmd.len() + 2);
        out.push("env".to_owned());
        out.push(self.mark.clone());
        out.extend(cmd.iter().cloned());
        out
    }
}

/// Lists the container's processes whose environment holds the mark (`$1`)
/// and kills each, never itself. Prints `ended:` and the process ids. Needs
/// `/proc` (Linux); exits 3 without it.
const END_SCRIPT: &str = r#"[ -r /proc/self/environ ] || { echo "no /proc in this environment"; exit 3; }
self=$$
found=""
for d in /proc/[0-9]*; do
  p=${d#/proc/}
  [ "$p" = "$self" ] && continue
  if tr '\000' '\n' < "$d/environ" 2>/dev/null | grep -qxF "$1"; then
    kill -9 "$p" 2>/dev/null && found="$found $p"
  fi
done
echo "ended:$found""#;

#[async_trait::async_trait]
impl ExternalExecutor for MarkedExecutor {
    async fn exec(&self, cmd: &[String], timeout: Duration) -> Result<ExternalOutcome, String> {
        self.inner.exec(&self.marked(cmd), timeout).await
    }

    async fn abandon(&self) {
        self.inner.abandon().await
    }

    fn report_grace(&self) -> Duration {
        self.inner.report_grace()
    }

    async fn sync_to_remote(&self) -> Result<(), String> {
        self.inner.sync_to_remote().await
    }
}

#[async_trait::async_trait]
impl AttemptProcesses for MarkedExecutor {
    fn delivered(&self) -> bool {
        true
    }

    async fn end(&self) -> Result<Vec<String>, String> {
        // Run unmarked, through the inner executor: the script's own
        // processes must not carry the mark it looks for.
        let cmd = vec![
            "sh".to_owned(),
            "-c".to_owned(),
            END_SCRIPT.to_owned(),
            "sh".to_owned(),
            self.mark.clone(),
        ];
        let out = self.inner.exec(&cmd, END_TIMEOUT).await?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let ended = stdout
            .lines()
            .find_map(|l| l.strip_prefix("ended:"))
            .filter(|_| out.exit_code == Some(0) && !out.timed_out);
        let Some(ended) = ended else {
            return Err(format!(
                "the task environment could not list them (exit {:?}{}): {} {}",
                out.exit_code,
                if out.timed_out { ", timed out" } else { "" },
                stdout.trim(),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        };
        let pids: Vec<&str> = ended.split_whitespace().collect();
        Ok(if pids.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "task environment: ended process(es) {} started under {}",
                pids.join(", "),
                self.mark
            )]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records each command; answers the end script with `answer`.
    struct Recording {
        seen: Mutex<Vec<Vec<String>>>,
        answer: &'static str,
    }

    #[async_trait::async_trait]
    impl ExternalExecutor for Recording {
        async fn exec(&self, cmd: &[String], _t: Duration) -> Result<ExternalOutcome, String> {
            self.seen.lock().expect("lock").push(cmd.to_vec());
            Ok(ExternalOutcome {
                stdout: self.answer.as_bytes().to_vec(),
                stderr: Vec::new(),
                exit_code: Some(0),
                timed_out: false,
            })
        }

        async fn sync_to_remote(&self) -> Result<(), String> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn every_routed_command_carries_the_attempt_and_the_end_script_does_not() {
        let inner = Arc::new(Recording {
            seen: Mutex::new(Vec::new()),
            answer: "ended: 731 732\n",
        });
        let marked = MarkedExecutor::new(inner.clone(), "run-1.c build-r0");
        marked
            .exec(
                &["pypi-server".into(), "run".into()],
                Duration::from_secs(1),
            )
            .await
            .expect("exec");
        let lines = AttemptProcesses::end(&marked).await.expect("end");
        let seen = inner.seen.lock().expect("lock").clone();
        assert_eq!(
            seen[0],
            vec![
                "env",
                "KGRAF_ATTEMPT=run-1.c_build-r0",
                "pypi-server",
                "run"
            ]
        );
        assert_eq!(&seen[1][..2], &["sh".to_owned(), "-c".to_owned()]);
        assert_eq!(seen[1][4], "KGRAF_ATTEMPT=run-1.c_build-r0");
        assert_eq!(
            lines,
            vec![
                "task environment: ended process(es) 731, 732 started under KGRAF_ATTEMPT=run-1.c_build-r0"
            ]
        );
        assert!(marked.delivered());
    }

    #[tokio::test]
    async fn an_environment_without_proc_is_an_error_not_silence() {
        let inner = Arc::new(Recording {
            seen: Mutex::new(Vec::new()),
            answer: "no /proc in this environment\n",
        });
        let marked = MarkedExecutor::new(inner, "w");
        let err = AttemptProcesses::end(&marked).await.expect_err("an error");
        assert!(err.contains("no /proc"), "{err}");
    }

    /// The end script itself, run by this machine's `sh` against a real
    /// marked background process (Linux only: it reads `/proc`).
    #[cfg(target_os = "linux")]
    #[test]
    fn the_end_script_kills_a_marked_process_and_spares_others() {
        use std::process::Command;
        let mark = "KGRAF_ATTEMPT=end-script-test";
        let mut marked = Command::new("env")
            .args([mark, "sleep", "300"])
            .spawn()
            .expect("spawn");
        let mut other = Command::new("sleep").arg("300").spawn().expect("spawn");
        std::thread::sleep(Duration::from_millis(100));
        let out = Command::new("sh")
            .args(["-c", END_SCRIPT, "sh", mark])
            .output()
            .expect("run");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains(&marked.id().to_string()),
            "{stdout} {:?}",
            out.stderr
        );
        assert!(marked.wait().is_ok());
        assert!(other.try_wait().expect("try_wait").is_none(), "spared");
        let _ = other.kill();
        let _ = other.wait();
    }
}
