//! Quality gates: the project's validation commands, discovered or configured,
//! run confined in a worktree of the integration commit they pin.

use crate::error::{Result, ResultExt};
use crate::workspaces::now_nanos;
use kgraf_orchestrator::{GateRequest, GateRunner, GateVerdict, UntrustedText};
use kgraf_sandbox::{
    CheckFolder, MergeBack, Redirect, SandboxPolicy, Workspace, WorkspaceManager, WorkspaceSpec,
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
///
/// A gate that does not apply is said on stderr, never dropped silently: the
/// pytest gate is left out, with the reason, when the project has Python
/// tests and pytest is in neither its virtual environment nor the
/// interpreter; the `npm test` gate is left out when the test script is the
/// one `npm init` writes, which only fails. A gate whose dependencies are not
/// installed is left out with the install it needs and the command that
/// makes it (decision 0087, amended 2026-10-06), and so is a gate whose tool
/// is not on `PATH`: either would fail every correct project.
pub fn discover_validation_commands(root: &Path, configured: &[String]) -> Result<Vec<String>> {
    let (found, notes) = discover_with(root, configured, &pytest_importable)?;
    for note in notes {
        eprintln!("{note}");
    }
    Ok(found)
}

/// The Python interpreter name the platform installs.
fn python_program() -> &'static str {
    kgraf_sandbox::python_program()
}

/// Whether the interpreter can import pytest.
pub fn pytest_importable() -> bool {
    let mut cmd = std::process::Command::new(python_program());
    cmd.args(["-c", "import pytest"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Under the spawn lock, as every spawn: see `kgraf_core::child_run::spawn_locked`.
    kgraf_core::child_run::status(&mut cmd).is_ok_and(|s| s.success())
}

/// Whether `program` is an executable file in a folder of `PATH`. The gate
/// runs with the host's `PATH`, so a tool that is not there fails the gate.
pub(crate) fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let names: Vec<String> = if cfg!(windows) {
        ["exe", "cmd", "bat"]
            .iter()
            .map(|ext| format!("{program}.{ext}"))
            .collect()
    } else {
        vec![program.to_owned()]
    };
    std::env::split_paths(&path).any(|dir| {
        names.iter().any(|n| {
            let file = dir.join(n);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::metadata(&file)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            }
            #[cfg(not(unix))]
            {
                file.is_file()
            }
        })
    })
}

/// What gate discovery asks of the machine, injected so tests decide it.
pub(crate) struct GateProbes<'a> {
    /// The host interpreter imports pytest.
    pub(crate) pytest_importable: &'a dyn Fn() -> bool,
    /// A program is on `PATH` ([`on_path`]).
    pub(crate) on_path: &'a dyn Fn(&str) -> bool,
    /// Where the package stores are.
    pub(crate) env: crate::deps::InstallEnv,
}

/// [`discover_validation_commands`] with the pytest import probe injected,
/// returning the commands and a note for each gate that does not apply.
pub(crate) fn discover_with(
    root: &Path,
    configured: &[String],
    pytest_importable: &dyn Fn() -> bool,
) -> Result<(Vec<String>, Vec<String>)> {
    discover_probed(
        root,
        configured,
        &GateProbes {
            pytest_importable,
            on_path: &on_path,
            env: crate::deps::InstallEnv::for_checkout(root),
        },
    )
}

/// [`discover_with`] with every probe injected.
pub(crate) fn discover_probed(
    root: &Path,
    configured: &[String],
    probes: &GateProbes<'_>,
) -> Result<(Vec<String>, Vec<String>)> {
    use crate::deps::Ecosystem;
    use crate::deps::installs::{GRADLE_FILES, is_dotnet_project};
    if !configured.is_empty() {
        return Ok((configured.to_vec(), Vec::new()));
    }
    let mut notes = Vec::new();
    let mut found = Vec::new();
    // Read once: the check walks the project and may ask the interpreter
    // whether it imports pytest.
    let all_missing = crate::deps::missing_installs(root, &probes.env, probes.pytest_importable);
    // A root gate needs its ecosystem's root installs; a .NET solution needs
    // every project restored, wherever it is.
    let missing = |ecosystem: Ecosystem| -> Vec<crate::deps::MissingInstall> {
        all_missing
            .iter()
            .filter(|m| {
                m.ecosystem == ecosystem
                    && (ecosystem == Ecosystem::Dotnet || m.dir.as_os_str().is_empty())
            })
            .cloned()
            .collect()
    };
    let not_installed = |gate: &str, missing: &[crate::deps::MissingInstall]| {
        let sentences: Vec<String> = missing.iter().map(|m| m.sentence()).collect();
        format!("{gate} gate not applicable: {}", sentences.join("; "))
    };
    let not_on_path = |gate: &str, program: &str| {
        format!(
            "{gate} gate not applicable: `{program}` is not on PATH, so the gate could not \
             run; install it or set gate commands"
        )
    };
    if root.join("Cargo.toml").is_file() {
        found.push("cargo test --workspace".to_owned());
    }
    let package = root.join("package.json");
    if package.is_file() {
        let text = std::fs::read_to_string(&package)
            .with_step(|| format!("cannot read {}", package.display()))?;
        let json: serde_json::Value = serde_json::from_str(&text)
            .with_settings(|| format!("{} is not valid JSON", package.display()))?;
        match json["scripts"]["test"].as_str() {
            // `npm init` writes a test script that only fails: a gate made of
            // it fails every correct project that has not replaced it.
            Some(script) if is_npm_init_test(script) => notes.push(format!(
                "npm test gate not applicable: {} has the test script `npm init` writes, which \
                 only fails (\"no test specified\"); set gate commands, or give the project a \
                 test script",
                package.display()
            )),
            Some(_) => {
                let missing = missing(Ecosystem::Npm);
                if missing.is_empty() {
                    found.push("npm test".to_owned());
                } else {
                    notes.push(not_installed("npm test", &missing));
                }
            }
            None => {}
        }
    }
    // A pyproject.toml alone is also what a Rust or Node project keeps its
    // tooling in (a formatter, a docs build): the pytest gate needs Python
    // tests, or a pytest configuration, to be plausible. The command is the
    // same with a virtual environment: the sandbox activates the root one,
    // so `python3` is its interpreter.
    if has_python_tests(root) {
        let missing = missing(Ecosystem::Python);
        if !missing.is_empty() {
            notes.push(format!(
                "pytest gate not applicable: {} has Python tests and names pytest, but pytest \
                 is in neither its virtual environment nor `{}`. {}",
                root.display(),
                python_program(),
                not_installed("pytest", &missing)
            ));
        } else if crate::deps::installs::pytest_available(root, probes.pytest_importable) {
            found.push(format!("{} -m pytest", python_program()));
        } else {
            notes.push(format!(
                "pytest gate not applicable: {} has Python tests, but pytest is neither named \
                 in its build files nor installed for `{}`; set gate commands to run them",
                root.display(),
                python_program()
            ));
        }
    }
    if root.join("go.mod").is_file() {
        let missing = missing(Ecosystem::Go);
        if !(probes.on_path)("go") {
            notes.push(not_on_path("go test", "go"));
        } else if !missing.is_empty() {
            notes.push(not_installed("go test", &missing));
        } else {
            found.push("go test ./...".to_owned());
        }
    }
    if root.join("pom.xml").is_file() {
        let missing = missing(Ecosystem::Maven);
        if !(probes.on_path)("mvn") {
            notes.push(not_on_path("Maven", "mvn"));
        } else if !missing.is_empty() {
            notes.push(not_installed("Maven", &missing));
        } else {
            // Offline: the network is off in the sandbox, and Maven would
            // otherwise try the remote repositories before it fails.
            found.push("mvn -o test".to_owned());
        }
    }
    if GRADLE_FILES.iter().any(|f| root.join(f).is_file()) {
        let missing = missing(Ecosystem::Gradle);
        if root.join("gradlew").is_file() {
            match gradle_wrapper_gate(root, probes.env.home.as_deref()) {
                Ok(()) if missing.is_empty() => found.push("./gradlew test --offline".to_owned()),
                Ok(()) => notes.push(not_installed("Gradle", &missing)),
                Err(why) => notes.push(format!("Gradle gate not applicable: {why}")),
            }
        } else if !(probes.on_path)("gradle") {
            notes.push(not_on_path("Gradle", "gradle"));
        } else if !missing.is_empty() {
            notes.push(not_installed("Gradle", &missing));
        } else {
            found.push("gradle test --offline".to_owned());
        }
    }
    let dotnet_root = std::fs::read_dir(root).ok().is_some_and(|entries| {
        entries.flatten().any(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.ends_with(".sln") || name.ends_with(".slnx") || is_dotnet_project(&name)
        })
    });
    if dotnet_root {
        let missing = missing(Ecosystem::Dotnet);
        let packages = probes
            .env
            .home
            .as_ref()
            .is_some_and(|h| h.join(".nuget/packages").is_dir());
        if !(probes.on_path)("dotnet") {
            notes.push(not_on_path(".NET", "dotnet"));
        } else if !missing.is_empty() {
            notes.push(not_installed(".NET", &missing));
        } else if !packages {
            notes.push(
                ".NET gate not applicable: there is no NuGet package folder \
                 (~/.nuget/packages); run `dotnet restore` in the project root"
                    .to_owned(),
            );
        } else {
            found.push("dotnet test".to_owned());
        }
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
    Ok((found, notes))
}

/// Whether the Gradle wrapper of the project at `root` can run offline in
/// the sandbox: its distribution is unpacked in the user's Gradle home, from
/// which the command's own Gradle home reaches it read-only
/// (`kgraf_sandbox::gradle_wrapper`). `Err` says why not, and what to do.
fn gradle_wrapper_gate(root: &Path, home: Option<&Path>) -> std::result::Result<(), String> {
    use kgraf_sandbox::gradle_wrapper::{installed_distributions, wrapper_distribution};
    let Some(name) = wrapper_distribution(root) else {
        return Err(format!(
            "{} names no Gradle distribution the wrapper can use (distributionUrl); set gate \
             commands to run Gradle another way",
            root.join("gradle/wrapper/gradle-wrapper.properties")
                .display()
        ));
    };
    let unpacked =
        home.is_some_and(|h| !installed_distributions(&h.join(".gradle"), &name).is_empty());
    if unpacked {
        Ok(())
    } else {
        Err(format!(
            "the Gradle wrapper's distribution {name} is not unpacked in \
             ~/.gradle/wrapper/dists; run `./gradlew --version` once in the project root, with \
             the network on"
        ))
    }
}

/// Whether `script` is the test script `npm init` writes,
/// `echo "Error: no test specified" && exit 1`, in any quoting.
fn is_npm_init_test(script: &str) -> bool {
    script.contains("no test specified") && script.contains("exit 1")
}

/// Whether the project at `root` has Python tests: a `tests` or `test` folder
/// holding a `test_*.py` or `*_test.py` file (searched a few levels down), or
/// a pytest configuration (`pytest.ini`, `[tool.pytest` in `pyproject.toml`,
/// `[tool:pytest]` in `setup.cfg`, `[pytest]` in `tox.ini`).
pub(crate) fn has_python_tests(root: &Path) -> bool {
    let configured = |file: &str, marker: &str| {
        std::fs::read_to_string(root.join(file)).is_ok_and(|t| t.contains(marker))
    };
    if root.join("pytest.ini").is_file()
        || configured("pyproject.toml", "[tool.pytest")
        || configured("setup.cfg", "[tool:pytest]")
        || configured("tox.ini", "[pytest]")
    {
        return true;
    }
    // Only when the project is Python at all: a `pyproject.toml`, a `setup.py`
    // or `setup.cfg`, or a `requirements.txt` says so.
    [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "requirements.txt",
    ]
    .iter()
    .any(|f| root.join(f).is_file())
        && ["tests", "test"]
            .iter()
            .any(|d| dir_has_python_test(&root.join(d), 3))
}

fn dir_has_python_test(dir: &Path, depth: usize) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if kind.is_file()
            && name.ends_with(".py")
            && (name.starts_with("test_") || name.ends_with("_test.py"))
        {
            return true;
        }
        // `file_type` does not follow links: a link to a folder is not entered.
        if kind.is_dir() && depth > 0 && dir_has_python_test(&entry.path(), depth - 1) {
            return true;
        }
    }
    false
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
///
/// Only the last `cap` bytes are read: a runaway log is bounded by seeking
/// to its end, so it never sits whole in the host's memory. A cut log starts
/// with a `[N bytes cut]` line.
fn read_log(log: &Path, cap: usize) -> String {
    use std::io::{Read, Seek, SeekFrom};
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
    // A few bytes past `cap` let the cut land on a character boundary.
    let keep = cap as u64 + 4;
    let mut skipped = meta.len().saturating_sub(keep);
    let mut reader = &file;
    if skipped > 0 && reader.seek(SeekFrom::Start(skipped)).is_err() {
        return refused().to_owned();
    }
    let mut bytes = Vec::new();
    let _ = reader.take(keep).read_to_end(&mut bytes);
    if skipped > 0 {
        // The read may start inside a multi-byte character.
        let lead = bytes
            .iter()
            .take(3)
            .take_while(|b| (**b & 0xC0) == 0x80)
            .count();
        bytes.drain(..lead);
        skipped += lead as u64;
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if skipped == 0 {
        return tail(&text, cap);
    }
    let mut start = text.len().saturating_sub(cap);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[{} bytes cut]\n{}", skipped + start as u64, &text[start..])
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
/// spinning escapee is eventually stopped). While it runs, the worktree is
/// not removed (`WorkspaceManager::remove` returns `Busy`, decision 0088);
/// the next startup cleanup removes it once the escapee has ended.
///
/// `checkout` is the checkout root the worktree was made from (the
/// directory holding `.kgraf`, `WorkspaceManager::repo`): cargo in the
/// worktree reads the checkout's `Cargo.toml` and `.cargo/config.toml` above
/// it, and those reads stop there (Linux; `with_ancestor_reads`). A cargo
/// file there that the sandbox refused is named, with its cause, after a
/// failing command's output.
///
/// The checkout's installed dependency trees (`node_modules/`, a `.venv/`,
/// `vendor/`) are linked into the worktree and readable, never writable
/// (`kgraf_sandbox::link_dependency_trees`, decision 0084 amended
/// 2026-10-06). After a failing command's output come a tree that could not
/// be linked, and each install the project needs and lacks, with the command
/// that makes it ([`crate::deps::installs`]): "Cannot find module" alone
/// reads like a fault in the work.
///
/// **The check's own folder.** Its temporary folder, every tool home and its
/// cargo target folder are in a new [`CheckFolder`] (`<base>/.checks/...`),
/// outside the worker's writable roots, never the workspace's scratch folder:
/// that one is the worker's `TMPDIR` and holds its build output, and what the
/// worker left there (npm's `.npmrc`, a Gradle or .NET home, a Go build
/// cache, cargo's build records and test binaries) could otherwise steer the
/// check, up to a forged pass (decision 0084, amended 2026-10-06). With
/// `warm`, the target folder's `debug` profile starts as a copy-on-write
/// clone of that build folder's (the user's own, which no sandboxed command
/// writes; decision 0060), so the check rebuilds only the project's own
/// crates; without it,
/// or where no clone can be made, the check builds cold, and a failing
/// check's output says so.
///
/// **The network.** Denied wherever the backend can deny it. On the Windows
/// backend, which cannot, the check runs with the network open and its output
/// ends with the notice that says so ([`kgraf_sandbox::CheckNetwork`],
/// decision 0011, amended 2026-10-06).
pub(crate) fn run_confined(
    ws: &Workspace,
    checkout: &Path,
    denies: &[PathBuf],
    command: &str,
    timeout: Duration,
    cap: usize,
    warm: Option<&Path>,
) -> std::result::Result<(bool, String), String> {
    run_confined_on(
        kgraf_sandbox::CheckNetwork::current(),
        ws,
        checkout,
        denies,
        command,
        timeout,
        cap,
        warm,
    )
}

/// [`run_confined`] with the network decision given, so a test can stand in
/// for a backend that cannot deny it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_confined_on(
    network: kgraf_sandbox::CheckNetwork,
    ws: &Workspace,
    checkout: &Path,
    denies: &[PathBuf],
    command: &str,
    timeout: Duration,
    cap: usize,
    warm: Option<&Path>,
) -> std::result::Result<(bool, String), String> {
    let links = kgraf_sandbox::link_dependency_trees(ws.root(), checkout);
    let check =
        CheckFolder::create(ws).map_err(|e| format!("cannot make the check's folder: {e}"))?;
    let target = check.target();
    let mut build_note = String::new();
    // Only the `debug` profile, the one a check's `cargo test` and `cargo
    // build` use: a clone costs one call per file, and a user's `target/`
    // also holds release builds, other targets and documentation.
    if let Some(warm) = warm.map(|w| w.join("debug"))
        && let Err(e) = kgraf_sandbox::clone_target_dir(&warm, &target.join("debug"))
    {
        build_note = format!(
            "\n[build] the check built cold: its build folder could not start from {}: {e}",
            warm.display()
        );
    }
    let tmp = check.tmp();
    let mut policy = SandboxPolicy::for_workspace(ws, network.network())
        // The Windows backend is always partial; a gate runs there under the
        // decision that opens its network, and says so (decision 0011).
        .accept_partial_isolation(network.accepts_partial_isolation())
        .with_writable_root(check.path().to_path_buf())
        .with_tmp_dir(tmp.clone())
        .with_ancestor_reads(checkout.to_path_buf())
        .with_dependency_links(&links);
    for d in denies {
        policy = policy.deny_read(d.clone());
    }
    // `cd svc/api && python -m pytest` runs with the virtual environment of
    // `svc/api`, the folder it works in.
    if let Some(dir) = kgraf_sandbox::script_start_dir(command) {
        policy = policy.with_command_dir(ws.root().join(dir));
    }
    let log = check.path().join(format!("gate-{}.log", now_nanos()));
    let mut cmd = sandboxed_command(&policy, "sh", ws.root())
        .map_err(|e| format!("cannot sandbox `{command}`: {e}"))?;
    let mut refused = refused_note(cmd.refused_reads());
    refused.push_str(&refused_note(&links.notes));
    // The log path goes through the environment, never through the shell's
    // parser, so a path with spaces or quotes cannot change the command.
    cmd.args(["-c", &format!("exec >\"$KGRAF_GATE_LOG\" 2>&1\n{command}")])
        .env("KGRAF_GATE_LOG", &log)
        .env("CARGO_TARGET_DIR", &target)
        // Python bytecode goes to the check's folder, outside the worktree, as
        // cargo's build output does: an import in the check writes no
        // `__pycache__` into the tree, and reads none the worker's run left.
        .env("PYTHONPYCACHEPREFIX", tmp.join("pycache"))
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
    // In the record of running children (`kgraf_core::waits::report`) until
    // this function returns.
    let _running = kgraf_core::waits::child_running(child.id(), &format!("gate: {command}"));
    let status = loop {
        // Unix: the leader is only ever reaped after the group kill below, so
        // its pid, the group id, stays reserved until every member is gone. A
        // poll that reaped first (`try_wait`) could let a background process
        // outlive the command.
        #[cfg(unix)]
        {
            match exited_unreaped(child.id()) {
                Ok(true) => {
                    kill_group_until_hold_free(child.id(), ws.hold_file());
                    match child.wait() {
                        Ok(status) => break Some(status),
                        Err(e) => return Err(format!("waiting for `{command}`: {e}")),
                    }
                }
                Ok(false) => {}
                Err(e) => return Err(format!("waiting for `{command}`: {e}")),
            }
        }
        #[cfg(not(unix))]
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(e) => return Err(format!("waiting for `{command}`: {e}")),
        }
        if started.elapsed() >= timeout {
            // The whole tree, bounded: a kill that fails or a tree that does
            // not end is the gate's error, never a wait without end.
            stop_tree(
                &mut GateTree(&mut child),
                command,
                kgraf_core::child_run::KILL_BOUND,
            )?;
            #[cfg(unix)]
            kill_group_until_hold_free(child.id(), ws.hold_file());
            // The tree has ended, so this only collects the status.
            match child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => return Err(format!("`{command}` still runs after its tree ended")),
                Err(e) => return Err(format!("waiting for `{command}`: {e}")),
            }
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
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
    // Windows: the command's leader has exited, but a process it started may
    // still write to the log, and dropping the child (which closes the job)
    // comes after this function reads it. End the tree first, so the log is
    // read whole and removed with nothing writing to it. Unix ends the group
    // above.
    #[cfg(windows)]
    let mut text = read_after_tree_ended(
        &mut GateTree(&mut child),
        command,
        kgraf_core::child_run::KILL_BOUND,
        || read_log(&log, cap),
    )?;
    #[cfg(not(windows))]
    let mut text = read_log(&log, cap);
    let _ = std::fs::remove_file(&log);
    // Said on every outcome, a pass included: a pass with the network open
    // proves less than one with it denied.
    let open = network.notice().map_or_else(String::new, open_network_note);
    match status {
        Some(s) => {
            text = format!("$ {command}\n{text}\n[{s}]");
            if !s.success() {
                text.push_str(&refused);
                text.push_str(&build_note);
                text.push_str(&missing_installs_note(checkout));
            }
            text.push_str(&open);
            Ok((s.success(), text))
        }
        None => Ok((
            false,
            format!(
                "$ {command}\n{text}\n[killed after {}s]{refused}{build_note}{open}",
                timeout.as_secs()
            ),
        )),
    }
}

/// A running command's process tree, as [`stop_tree`] ends it.
trait Tree {
    /// Signal every process of the tree to end.
    fn kill_tree(&mut self) -> std::io::Result<()>;
    /// Whether the tree has ended. Unix: whether the leader has exited,
    /// without reaping it, so its group id stays reserved for the group
    /// kill that follows.
    fn tree_ended(&mut self) -> std::io::Result<bool>;
}

/// A gate's sandboxed command: on Unix a process group the leader's pid
/// names (the sandbox calls `setsid` before exec), on Windows a job object.
struct GateTree<'a, C>(&'a mut C);

#[cfg(unix)]
impl Tree for GateTree<'_, std::process::Child> {
    fn kill_tree(&mut self) -> std::io::Result<()> {
        let pid = self.0.id() as libc::pid_t;
        let gone = |e: &std::io::Error| e.raw_os_error() == Some(libc::ESRCH);
        // SAFETY: the leader is not reaped (only `run_confined_on` reaps it,
        // after this), so its pid names this child and its group.
        if unsafe { libc::killpg(pid, libc::SIGKILL) } != 0 {
            let e = std::io::Error::last_os_error();
            if !gone(&e) {
                return Err(e);
            }
        }
        // The leader too, in case it left its group. SAFETY: as above.
        if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
            let e = std::io::Error::last_os_error();
            if !gone(&e) {
                return Err(e);
            }
        }
        Ok(())
    }

    fn tree_ended(&mut self) -> std::io::Result<bool> {
        exited_unreaped(self.0.id())
    }
}

#[cfg(windows)]
impl Tree for GateTree<'_, kgraf_sandbox::SandboxedChild> {
    fn kill_tree(&mut self) -> std::io::Result<()> {
        self.0.kill()
    }

    fn tree_ended(&mut self) -> std::io::Result<bool> {
        Ok(self.0.active_processes()? == 0)
    }
}

/// Kill `tree` and wait, at most `bound`, until it has ended. A kill that
/// fails, or a tree still running at the bound, is an error naming
/// `command`. Polls every 10 ms: this runs only on a timeout, never on a
/// gate's normal path.
fn stop_tree(
    tree: &mut impl Tree,
    command: &str,
    bound: Duration,
) -> std::result::Result<(), String> {
    tree.kill_tree()
        .map_err(|e| format!("cannot kill `{command}` and what it started: {e}"))?;
    let deadline = std::time::Instant::now() + bound;
    loop {
        match tree.tree_ended() {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => return Err(format!("waiting for `{command}` to end: {e}")),
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "`{command}` and what it started had not ended {bound:?} after the kill"
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// End `tree`, waiting at most `bound` ([`stop_tree`]), and only then run
/// `read`, which reads what the tree wrote.
#[cfg(any(windows, test))]
fn read_after_tree_ended<R>(
    tree: &mut impl Tree,
    command: &str,
    bound: Duration,
    read: impl FnOnce() -> R,
) -> std::result::Result<R, String> {
    stop_tree(tree, command, bound)?;
    Ok(read())
}

/// The line a check that ran with the network open ends with.
fn open_network_note(notice: &str) -> String {
    format!("\n[sandbox] {notice}")
}

/// Each install the checkout needs and lacks, as lines to append to a
/// failing command's output, or nothing. Read only after a failure: the
/// check reads manifests and folders and costs a walk of the checkout.
fn missing_installs_note(checkout: &Path) -> String {
    let missing = crate::deps::missing_installs(
        checkout,
        &crate::deps::InstallEnv::for_checkout(checkout),
        &pytest_importable,
    );
    missing
        .iter()
        .map(|m| format!("\n[dependencies] {}", m.sentence()))
        .collect()
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
            let name = crate::workspaces::harness_worktree_name(crate::workspaces::GATE_PREFIX);
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
                // The gate builds cold: its worktree is new, and so is its build.
                match run_confined(&ws, manager.repo(), &denies, command, timeout, cap, None) {
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

#[cfg(test)]
mod stop_tree_tests {
    use super::{Tree, read_after_tree_ended, stop_tree};
    use std::time::{Duration, Instant};

    struct Fake {
        kill: Option<std::io::ErrorKind>,
        ends_after: Option<Duration>,
        killed_at: Option<Instant>,
        /// Set when `tree_ended` first reports the end.
        ended: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl Tree for Fake {
        fn kill_tree(&mut self) -> std::io::Result<()> {
            if let Some(kind) = self.kill {
                return Err(std::io::Error::new(kind, "refused"));
            }
            self.killed_at = Some(Instant::now());
            Ok(())
        }

        fn tree_ended(&mut self) -> std::io::Result<bool> {
            let ended = match (self.ends_after, self.killed_at) {
                (Some(after), Some(at)) => at.elapsed() >= after,
                _ => false,
            };
            if ended {
                self.ended.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(ended)
        }
    }

    fn fake(kill: Option<std::io::ErrorKind>, ends_after: Option<Duration>) -> Fake {
        Fake {
            kill,
            ends_after,
            killed_at: None,
            ended: Default::default(),
        }
    }

    /// [`stop_tree`] on its own thread: a stop that never returns fails the
    /// test instead of hanging the suite.
    fn stop(mut tree: Fake, bound: Duration) -> Result<(), String> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(stop_tree(&mut tree, "cargo test", bound));
        });
        rx.recv_timeout(Duration::from_secs(20))
            .expect("stop_tree returned within its bound")
    }

    #[test]
    fn a_failed_kill_is_an_error_naming_the_command_at_once() {
        let started = Instant::now();
        let err = stop(
            fake(Some(std::io::ErrorKind::PermissionDenied), None),
            Duration::from_secs(10),
        )
        .expect_err("kill failed");
        assert!(err.contains("cannot kill `cargo test`"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_tree_that_does_not_end_is_an_error_at_the_bound() {
        let started = Instant::now();
        let err = stop(fake(None, None), Duration::from_millis(200)).expect_err("never ends");
        assert!(
            err.contains("`cargo test`") && err.contains("had not ended"),
            "{err}"
        );
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(200) && took < Duration::from_secs(5));
    }

    /// The log is read only once the tree has ended: a process the command
    /// left behind could still be writing to it before.
    #[test]
    fn the_log_is_read_after_the_tree_has_ended() {
        let mut tree = fake(None, Some(Duration::from_millis(50)));
        let ended = tree.ended.clone();
        let seen_ended =
            read_after_tree_ended(&mut tree, "cargo test", Duration::from_secs(10), || {
                ended.load(std::sync::atomic::Ordering::SeqCst)
            });
        assert_eq!(seen_ended, Ok(true), "read while the tree still ran");
    }

    #[test]
    fn a_tree_that_does_not_end_is_not_read() {
        let mut tree = fake(None, None);
        let mut read = false;
        let got =
            read_after_tree_ended(&mut tree, "cargo test", Duration::from_millis(100), || {
                read = true;
            });
        assert!(got.is_err());
        assert!(!read, "read a log a process may still write");
    }

    #[test]
    fn a_tree_that_ends_after_the_kill_is_stopped() {
        stop(
            fake(None, Some(Duration::from_millis(50))),
            Duration::from_secs(10),
        )
        .expect("ended");
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{gradle_wrapper_gate, read_log};

    /// The wrapper gate holds only when the project names its distribution
    /// and the user's Gradle home has a finished unpack of it.
    #[test]
    fn the_gradle_wrapper_gate_needs_a_named_and_unpacked_distribution() {
        let project = tempfile::tempdir().expect("project");
        let home = tempfile::tempdir().expect("home");
        let p = project.path();
        let h = home.path();
        let err = gradle_wrapper_gate(p, Some(h)).expect_err("no wrapper");
        assert!(err.contains("names no Gradle distribution"), "{err}");
        let props = p.join("gradle/wrapper/gradle-wrapper.properties");
        std::fs::create_dir_all(props.parent().expect("parent")).expect("mkdir");
        std::fs::write(
            &props,
            "distributionUrl=https\\://services.gradle.org/distributions/gradle-8.5-bin.zip\n",
        )
        .expect("write");
        let err = gradle_wrapper_gate(p, Some(h)).expect_err("nothing unpacked");
        assert!(err.contains("gradle-8.5-bin is not unpacked"), "{err}");
        let err = gradle_wrapper_gate(p, None).expect_err("no home");
        assert!(err.contains("is not unpacked"), "{err}");
        let dist = h.join(".gradle/wrapper/dists/gradle-8.5-bin/abc123");
        std::fs::create_dir_all(&dist).expect("mkdir");
        std::fs::write(dist.join("gradle-8.5-bin.zip.ok"), "").expect("write");
        let err = gradle_wrapper_gate(p, Some(h)).expect_err("a mark without a folder");
        assert!(err.contains("is not unpacked"), "{err}");
        std::fs::create_dir_all(dist.join("gradle-8.5")).expect("mkdir");
        assert_eq!(gradle_wrapper_gate(p, Some(h)), Ok(()));
    }

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
        let text = read_log(&log, 1024);
        assert!(!text.contains("SECRET"), "{text}");
        assert!(text.contains("not read"), "{text}");

        let hard = scratch.join("gate-2.log");
        std::fs::hard_link(outside.join("secret"), &hard).expect("link");
        assert!(!read_log(&hard, 1024).contains("SECRET"));

        let plain = scratch.join("gate-3.log");
        std::fs::write(&plain, "ok\n").expect("write");
        assert_eq!(read_log(&plain, 1024), "ok\n");
        assert_eq!(read_log(&scratch.join("absent.log"), 1024), "");
    }

    /// A log far larger than the cap is read from its end only: the result is
    /// bounded, keeps the tail, and says how much was cut.
    #[test]
    fn a_huge_log_is_read_from_its_end_within_the_cap() {
        use std::io::{Seek, SeekFrom, Write};
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("gate-9.log");
        let mut f = std::fs::File::create(&log).expect("create");
        // A sparse 64 MiB of zeros, then a marker line at the end.
        f.set_len(64 * 1024 * 1024).expect("len");
        f.seek(SeekFrom::End(0)).expect("seek");
        f.write_all("h\u{e9}llo END\n".as_bytes()).expect("write");
        drop(f);
        let text = read_log(&log, 4096);
        assert!(text.len() < 4096 + 64, "{}", text.len());
        assert!(text.starts_with("[") && text.contains("bytes cut]"));
        assert!(text.ends_with("h\u{e9}llo END\n"));
    }

    /// The refused reads are lines for the check's output, one each.
    #[test]
    fn refused_reads_are_one_sandbox_line_each() {
        assert_eq!(super::refused_note(&[]), "");
        assert_eq!(
            super::refused_note(&["a".to_owned(), "b".to_owned()]),
            "\n[sandbox] a\n[sandbox] b"
        );
    }

    /// The count of cut bytes is exact: the file's size less what is kept,
    /// whether the cut falls on a character or inside one.
    #[test]
    fn the_cut_byte_count_is_exact() {
        let dir = tempfile::tempdir().expect("dir");
        let log = dir.path().join("gate-1.log");
        let ten = "y".repeat(10);
        // Cut on a character boundary.
        std::fs::write(&log, "x".repeat(100)).expect("write");
        assert_eq!(
            read_log(&log, 10),
            format!("[90 bytes cut]\n{}", "x".repeat(10))
        );
        // The read starts inside a two-byte character.
        std::fs::write(&log, format!("{}\u{e9}{}", "x".repeat(20), "y".repeat(13))).expect("write");
        assert_eq!(read_log(&log, 10), format!("[25 bytes cut]\n{ten}"));
        // The cut itself falls inside a two-byte character: it moves on to
        // the next boundary.
        std::fs::write(
            &log,
            format!("{}abc\u{e9}{}", "x".repeat(50), "y".repeat(9)),
        )
        .expect("write");
        assert_eq!(
            read_log(&log, 10),
            format!("[55 bytes cut]\n{}", "y".repeat(9))
        );
        // A short file that begins with stray continuation bytes is shown
        // as it is; nothing is cut.
        std::fs::write(&log, [0x80u8, b'a']).expect("write");
        assert_eq!(read_log(&log, 10), "\u{fffd}a");
        // Short files are cut by `tail` alone.
        std::fs::write(&log, "abcdef").expect("write");
        assert_eq!(read_log(&log, 6), "abcdef");
        std::fs::write(&log, "abcdefg").expect("write");
        assert_eq!(read_log(&log, 6), "[1 bytes cut]\nbcdefg");
        std::fs::write(&log, "\u{e9}abcd").expect("write");
        assert_eq!(read_log(&log, 5), "[2 bytes cut]\nabcd");
        assert_eq!(super::tail("abcdef", 6), "abcdef");
        assert_eq!(super::tail("abcdefg", 6), "[1 bytes cut]\nbcdefg");
    }
}
