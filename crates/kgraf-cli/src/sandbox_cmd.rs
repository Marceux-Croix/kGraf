//! `kgraf sandbox selfcheck` (hidden): run one command under the network
//! policy a dependency install gets, with a hidden folder, and report whether
//! the sandbox held.
//!
//! The command runs in kGraf's sandbox with network only through the egress
//! proxy and a hidden folder inside its writable tree. It must reach an
//! allowed origin through the proxy, fail to reach the same origin directly,
//! write in the tree, and fail to write in the hidden folder. On Linux this
//! needs unprivileged user namespaces; where the system denies them the
//! sandbox refuses the policy and this command exits with that error, which
//! names the fix. The Linux self-test workflow uses it on an installed
//! package. It is for diagnosis, not a feature of a run, so `--help` does not
//! list it.
//!
//! On Windows the probe runs through Git Bash (the sandbox starts `sh` there)
//! and the check prints which backend ran. With the sandbox user (after
//! `kgraf sandbox setup`) a direct connection must fail, like on Unix. With the
//! token sandbox the network cannot be confined (decision 0011): the check
//! runs with the network open, prints a `network: not confined` line that is
//! not a pass, and still exits 0 when the other steps pass.
//!
//! With the sandbox user the check also proves DNS confinement: a name lookup
//! by the sandbox user goes through the DNS Client service, which the firewall
//! rules do not see. The `dns:` line passes only when the lookup reached no
//! server and the service did not look the name up
//! (`kgraf_sandbox::sandbox_user::dns_check`); anything else is a failed line
//! and the check exits with an error.

use anyhow::{Result, bail};
use kgraf_sandbox::Backend;
use kgraf_sandbox::sandbox_user::dns_check::DnsVerdict;

/// The marker the local origin answers with.
const ORIGIN_BODY: &str = "selfcheck-ok";

/// The probe, run with `sh -c`. Arguments: tree, hidden folder, origin port.
/// Exit codes tell the failing step: 11 tree not writable, 12 hidden folder
/// writable, 13 proxy did not reach the origin, 14 a direct connection
/// reached the origin. On Windows `sh` is Git Bash, whose `curl` is used.
const PROBE: &str = r#"
tree="$1"; hidden="$2"; port="$3"
echo inside > "$tree/selfcheck-write" || exit 11
if ( echo x > "$hidden/selfcheck-write" ) 2>/dev/null; then exit 12; fi
curl -sS --max-time 10 "http://127.0.0.1:$port/" | grep -q '__BODY__' || exit 13
if curl -sS --max-time 5 --noproxy '*' "http://127.0.0.1:$port/" >/dev/null 2>&1; then exit 14; fi
exit 0
"#;

/// What each probe exit code means.
fn explain(code: Option<i32>) -> String {
    match code {
        Some(11) => "the command could not write in its tree".into(),
        Some(12) => "the hidden folder was writable".into(),
        Some(13) => {
            "the command did not reach the allowed origin through the proxy (is curl installed?)"
                .into()
        }
        Some(14) => {
            "a direct connection to the origin worked; only the proxy should be reachable".into()
        }
        Some(c) => format!("the probe exited with status {c}"),
        None => "the probe was killed by a signal".into(),
    }
}

/// What the check proved about the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetworkResult {
    /// A direct connection failed and the proxy worked.
    Confined,
    /// The backend cannot confine the network, so nothing was proved. Not a
    /// pass.
    NotConfined,
}

/// The backend's name as the check prints it.
fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::MacosSeatbelt => "macOS Seatbelt",
        Backend::LinuxLandlock => "Linux Landlock + seccomp",
        Backend::WindowsRestrictedToken => "Windows restricted token (token sandbox)",
        Backend::WindowsSandboxUser => "Windows sandbox user",
    }
}

/// Whether `backend` cannot confine the network at all (decision 0011): the
/// Windows token sandbox. There the check runs the command with the network
/// open and proves nothing about it.
fn backend_leaves_network_open(backend: Backend) -> bool {
    backend == Backend::WindowsRestrictedToken
}

/// The verdict for a probe exit code under `backend`. A failure names the
/// failing step. Exit 14 (a direct connection worked) is a failure where the
/// backend confines the network, and expected where it does not. Where it
/// does not, a pass never claims the network: it reports `NotConfined`,
/// whatever the direct connection did.
fn verdict(backend: Backend, code: Option<i32>) -> Result<NetworkResult, String> {
    let open = backend_leaves_network_open(backend);
    match code {
        Some(0) if open => Ok(NetworkResult::NotConfined),
        Some(0) => Ok(NetworkResult::Confined),
        Some(14) if open => Ok(NetworkResult::NotConfined),
        other => Err(explain(other)),
    }
}

/// What the check learned about name lookups.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DnsResult {
    /// No DNS step for this backend: on macOS and Linux a proxy-only command
    /// reaches no resolver by construction (no resolver socket, no route out
    /// of the network namespace), and this check does not probe it. No line.
    NotChecked,
    /// The token sandbox leaves the network, and so DNS, open.
    OpenUnderTokenSandbox,
    /// The sandbox user: judged from a real lookup. Built only on Windows;
    /// the tests build it on every host.
    #[cfg_attr(not(windows), allow(dead_code))]
    Judged(DnsVerdict),
}

impl DnsResult {
    /// Whether this result fails the whole check.
    fn failed(&self) -> bool {
        matches!(self, DnsResult::Judged(v) if !v.passed())
    }

    fn line(&self) -> Option<String> {
        match self {
            DnsResult::NotChecked => None,
            DnsResult::OpenUnderTokenSandbox => {
                Some("  dns: not confined (token sandbox; run `kgraf sandbox setup`)".to_owned())
            }
            DnsResult::Judged(v) => Some(format!("  {v}")),
        }
    }
}

/// The lines printed once the probe passed. The first is
/// `sandbox selfcheck: ok` only when every step was proved, and
/// `sandbox selfcheck: failed` when the DNS step failed or proved nothing.
fn report_lines(backend: Backend, network: NetworkResult, dns: &DnsResult) -> Vec<String> {
    let first = if dns.failed() {
        "sandbox selfcheck: failed"
    } else if network == NetworkResult::Confined && *dns != DnsResult::OpenUnderTokenSandbox {
        "sandbox selfcheck: ok"
    } else {
        "sandbox selfcheck: passed with a gap"
    };
    let net = match network {
        NetworkResult::Confined => "  network: only the egress proxy was reachable",
        NetworkResult::NotConfined => {
            "  network: not confined (token sandbox; run `kgraf sandbox setup`)"
        }
    };
    let mut lines = vec![
        first.to_owned(),
        format!("  backend: {}", backend_name(backend)),
        net.to_owned(),
    ];
    lines.extend(dns.line());
    lines.push("  hidden folder: not writable".to_owned());
    lines.push("  tree: writable".to_owned());
    lines
}

/// The DNS step under the sandbox user: the owner looks up a control name,
/// the sandbox user looks up a probe name under `policy`, and the owner asks
/// the DNS Client cache about both and about a third, new name. The verdict
/// is `kgraf_sandbox::sandbox_user::dns_check::judge_settled`, which reads
/// the cache a second time where the first reading passed.
#[cfg(windows)]
fn dns_check(
    proxy: &kgraf_sandbox::egress::EgressProxy,
    policy: &kgraf_sandbox::SandboxPolicy,
    tree: &std::path::Path,
) -> Result<DnsVerdict> {
    use anyhow::Context;
    use kgraf_sandbox::sandbox_user::dns_check::{
        CACHE_SETTLE, DnsEvidence, PROBE_BOUND, judge_settled, parse_probe, probe_name,
        probe_script,
    };
    use kgraf_sandbox::sandbox_user::windows::dns::query;
    use kgraf_sandbox::{Redirect, sandboxed_command};

    let fresh = || probe_name().context("make a probe name");
    let (control, probe, unseen) = (fresh()?, fresh()?, fresh()?);
    let control_status = query(&control, false);
    let script = probe_script(&probe).context("the probe name")?;
    // Windows PowerShell 5.1 ships with every supported Windows at a fixed
    // path, so no PATH lookup decides which program runs.
    let system_root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let powershell =
        std::path::Path::new(&system_root).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
    let mut cmd = sandboxed_command(policy, &powershell, tree)
        .context("the sandbox refused the DNS probe")?;
    for (k, v) in proxy.env() {
        cmd.env(k, v);
    }
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped);
    // Bounded here, not by the generic two-hour WAIT_BOUND: a probe that
    // hangs must not hold selfcheck for the rest of the day. A timeout, and
    // a probe that would not start, are `dns: not proved` with the reason,
    // never an error that throws away the lines the check already proved.
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            return Ok(DnsVerdict::NotProved(format!(
                "the lookup in the sandbox would not start: {e}"
            )));
        }
    };
    let out = match child.wait_with_output_within(PROBE_BOUND) {
        Ok(out) => out,
        Err(e) => {
            return Ok(DnsVerdict::NotProved(format!(
                "the lookup in the sandbox did not finish within {} seconds: {e}",
                PROBE_BOUND.as_secs()
            )));
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    // No result means the output carried no end marker or no status at all:
    // the probe did not run, or its tail was cut off. Either way it is not
    // evidence, and `judge` turns this into `dns: not proved`.
    let probe_result = parse_probe(&stdout).ok_or_else(|| {
        format!(
            "the lookup printed no whole result (no {} line, or no status): status {:?}; {} {}",
            kgraf_sandbox::sandbox_user::dns_check::END_MARK,
            out.status.code(),
            stdout.trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    });
    // The cache is read after the probe's process has exited, which can be
    // before a query that left the machine has its answer, so a passing
    // first reading is taken again after CACHE_SETTLE.
    Ok(judge_settled(
        CACHE_SETTLE,
        || DnsEvidence {
            control: control_status,
            probe: probe_result.clone(),
            cache_known: query(&control, true),
            cache_fresh: query(&unseen, true),
            cache_probe: query(&probe, true),
        },
        std::thread::sleep,
    ))
}

/// A listener for the local origin on a port the command must not reach
/// directly. The offline sandbox user's firewall rules leave kGraf's egress
/// proxy ports open on loopback, so an origin that landed in that range
/// would answer the probe's direct connection and fail the check for no
/// fault of the sandbox.
fn bind_origin() -> std::io::Result<std::net::TcpListener> {
    use std::net::{Ipv4Addr, TcpListener};

    let mut held = Vec::new();
    for _ in 0..64 {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        if !kgraf_sandbox::sandbox_user::PROXY_PORTS.contains(&port) {
            return Ok(listener);
        }
        // Hold it, so the next try cannot be given the same port again.
        held.push(listener);
    }
    Err(std::io::Error::other(
        "every try bound a port kGraf's egress proxy reserves",
    ))
}

/// Start `cmd` and wait for it, bounded by `WAIT_BOUND`. On Linux the proxy
/// takes the listener the command opened in its namespace; elsewhere the
/// proxy already listens. The DNS probe does not use this: it has its own,
/// much shorter bound ([`PROBE_BOUND`]).
fn run_command(
    proxy: &kgraf_sandbox::egress::EgressProxy,
    cmd: &mut kgraf_sandbox::SandboxedCommand,
) -> Result<std::process::Output> {
    use anyhow::Context;
    #[cfg(unix)]
    let child = proxy.spawn(cmd);
    #[cfg(windows)]
    let child = {
        let _ = proxy;
        cmd.spawn()
    };
    let child = child.context(
        "start the sandboxed command (on Windows this needs Git for Windows: \
         install it with `winget install Git.Git` or set KGRAF_GIT_BASH_PATH)",
    )?;
    child.wait_with_output().context("wait for the command")
}

/// Run the check. Prints one line per step and returns an error when the
/// sandbox refuses the policy or a step fails. Exit status: success when every
/// step passed, including a pass with a gap under a backend that cannot
/// confine the network (the output says so); an error for any failed step.
pub fn selfcheck() -> Result<()> {
    use anyhow::Context;
    use kgraf_sandbox::egress::{EgressProxy, HostRule};
    use kgraf_sandbox::{Network, Redirect, SandboxPolicy, sandboxed_command};
    use std::io::{Read, Write};

    let backend = kgraf_sandbox::platform_isolation()
        .context("the sandbox has no backend on this machine")?
        .backend;
    // On every way out, a pass or a failure: what the launch path made and
    // measured, on standard error so the report lines above stay as they are.
    #[cfg(windows)]
    let _launch_record = PrintLaunchRecord;

    let origin = bind_origin().context("bind the local origin")?;
    let port = origin.local_addr().context("origin address")?.port();
    std::thread::spawn(move || {
        for client in origin.incoming() {
            let Ok(mut c) = client else { continue };
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf);
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{ORIGIN_BODY}",
                ORIGIN_BODY.len()
            );
        }
    });
    let proxy = EgressProxy::start(vec![
        HostRule::parse(&format!("127.0.0.1:{port}")).map_err(anyhow::Error::msg)?,
    ])
    .context("start the egress proxy")?;

    let dir = tempfile::tempdir().context("temporary tree")?;
    let tree = kgraf_core::plain_path::canonicalize(dir.path()).context("canonical tree")?;
    let hidden = tree.join("hidden");
    std::fs::create_dir(&hidden).context("hidden folder")?;
    // The token sandbox refuses a proxy-only network and partial isolation.
    // The check runs it with the network open and accepts that, then says so.
    let open = backend_leaves_network_open(backend);
    let network = if open {
        Network::Allow
    } else {
        proxy.network()
    };
    let policy = SandboxPolicy::new(vec![tree.clone()], network)
        .hide(hidden.clone())
        .accept_partial_isolation(open);

    let mut cmd = sandboxed_command(&policy, "sh", &tree)
        .context("the sandbox refused a proxy-limited command with a hidden folder")?;
    for (k, v) in proxy.env() {
        cmd.env(k, v);
    }
    cmd.arg("-c")
        .arg(PROBE.replace("__BODY__", ORIGIN_BODY))
        .arg("probe")
        .arg(&tree)
        .arg(&hidden)
        .arg(port.to_string())
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped);
    let out = run_command(&proxy, &mut cmd)?;
    match verdict(backend, out.status.code()) {
        Ok(network) => {
            let dns = match backend {
                Backend::WindowsRestrictedToken => DnsResult::OpenUnderTokenSandbox,
                #[cfg(windows)]
                Backend::WindowsSandboxUser => {
                    DnsResult::Judged(dns_check(&proxy, &policy, &tree)?)
                }
                _ => DnsResult::NotChecked,
            };
            for line in report_lines(backend, network, &dns) {
                println!("{line}");
            }
            #[cfg(windows)]
            print_named_object_lines(backend);
            if dns.failed() {
                bail!(
                    "sandbox selfcheck failed (backend: {}): {}",
                    backend_name(backend),
                    dns.line().unwrap_or_default().trim()
                );
            }
            Ok(())
        }
        Err(why) => bail!(
            "sandbox selfcheck failed (backend: {}): {why}\n{}{}",
            backend_name(backend),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

// ---- the Windows sandbox user (decision 0097) -------------------------------

/// Prints the launch record (`kgraf_sandbox::sandbox_user::launch_record`)
/// to standard error when dropped: the window stations made, the logon
/// calls' durations and the spawn lock's use. A Windows run sends these
/// lines back with the selfcheck output.
#[cfg(windows)]
struct PrintLaunchRecord;

#[cfg(windows)]
impl Drop for PrintLaunchRecord {
    fn drop(&mut self) {
        for line in kgraf_sandbox::sandbox_user::launch_record::lines() {
            eprintln!("  {line}");
        }
    }
}

// ---- which launch path a sandboxed command takes (decision 0097) ------------
//
// Setup decides it in the owner's own process, after the administrator step
// has run and the passwords are stored. Everything in this section but the
// two closures `decide_launch_path` is given is platform-neutral, so the
// order, every message and every record written are tested on every host.

use std::collections::BTreeMap;
use std::path::Path;

use kgraf_sandbox::sandbox_user::handshake::{Event, StepOutcome};
use kgraf_sandbox::sandbox_user::plan::StepId;
use kgraf_sandbox::sandbox_user::session0::{Active, Attempt, Proof, select};
use kgraf_sandbox::sandbox_user::{named_object_grant, probe, store};

/// The elevated action that writes the fallback grant and nothing else. Its
/// own action, so administrator rights are asked for a second time only on a
/// machine where the launcher service did not prove itself.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
const GRANT_ACTION: &str = "grant-named-objects";

/// What setup says before it measures the launch path.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
const PROBE_INTRO: &str = "kGraf now measures whether a sandboxed command can make the named \
                           kernel objects cargo and rustc need.";

/// What setup says where a probe could not run at all. No administrator
/// rights are asked for: a probe that did not run says nothing about named
/// objects, and must not move the machine to the weaker fallback.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
const COULD_NOT_RUN_NOTICE: &str = "The probe could not run, so nothing is known about named objects on this machine. kGraf \
     does not ask for administrator rights for the fallback: a probe that did not run is no \
     reason to admit a sandboxed command to the object directory of your own Windows session. \
     kGraf's launcher service stays the launch path, unproved. `kgraf sandbox selfcheck` \
     measures it again.";

/// Keep every step's outcome as the administrator step reports it, so the
/// owner's process reads what a step did without looking at the machine a
/// second time.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
fn record_outcome(outcomes: &mut BTreeMap<String, StepOutcome>, event: &Event) {
    if let Event::Step { key, outcome } = event {
        outcomes.insert(key.clone(), *outcome);
    }
}

/// Why the step `key` did not finish, or `None` where it did.
///
/// A step the administrator run never reported is a failure as much as one
/// that reported a part of its work: either way kGraf cannot say the step is
/// on the machine, and a path it cannot say is there must promise nothing.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
fn step_failure(outcomes: &BTreeMap<String, StepOutcome>, key: &str) -> Option<String> {
    match outcomes.get(key) {
        None => Some(format!(
            "the administrator step did not report the {key} step"
        )),
        Some(StepOutcome::Partial) => Some(format!("the {key} step finished only in part")),
        Some(outcome @ (StepOutcome::Removed | StepOutcome::AlreadyGone)) => Some(format!(
            "the {key} step reported {outcome}, which a setup never does"
        )),
        Some(StepOutcome::Created | StepOutcome::Updated | StepOutcome::Verified) => None,
    }
}

/// The record at `store`, or the path that promises the least where there is
/// none or it cannot be read. The same rule a sandboxed command's own launch
/// follows, so `status` never says more than a command gets.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
fn read_active(store: &Path) -> Active {
    std::fs::read_to_string(store::launch_path_file(store))
        .map(|text| Active::decode(&text))
        .unwrap_or_else(|_| Active::decode(""))
}

/// Write the record beside the credential store, through a temporary file so
/// a failed write never leaves half a record.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
fn write_active(store: &Path, active: &Active) -> Result<()> {
    use anyhow::Context;
    let path = store::launch_path_file(store);
    let tmp = path.with_extension("launch.tmp");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    std::fs::write(&tmp, active.encode()).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// The line setup prints for the record it just wrote, before it measures
/// that path.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
fn recorded_words(active: &Active) -> String {
    format!(
        "  recorded     the launch path {}, not yet proved",
        active.path.key()
    )
}

/// The line setup prints for what a probe found.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
fn probe_words(proof: &Proof) -> String {
    match proof {
        Proof::Made => "  proved       a sandbox user made a named semaphore and an unprefixed \
                        named mutex through this path"
            .to_owned(),
        Proof::Refused(why) => format!("  refused      a named object through this path: {why}"),
        Proof::CouldNotRun(why) => format!(
            "  not proved   the probe could not run, which says nothing about named objects: \
             {why}"
        ),
    }
}

/// What setup says before it asks for administrator rights the second time:
/// what failed, that more rights are needed, and what the fallback costs.
/// Said before the prompt, never after it, so nobody is asked to allow
/// something they have not been told the price of.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
fn fallback_notice(why: &str) -> String {
    format!(
        "kGraf's launcher service could not make the named kernel objects a build needs: {why}\n\
         kGraf needs administrator rights once more, for the fallback alone. What it would do, \
         and what it costs:\n{}\nThe rights it adds: {}.\nWithout the fallback, cargo and rustc \
         cannot run as a sandbox user; everything else in the sandbox works as it does now.",
        named_object_grant::EXPLANATION,
        named_object_grant::mask_text()
    )
}

/// Decide and record which launch path a sandboxed command takes, and say
/// every step of it out loud.
///
/// The order: the launcher service first, because it needs no grant; the
/// fallback only where a probe showed a named object refused through the
/// service. `run_probe` runs the probe as a sandbox user through whatever
/// the record says now, so the record is written before each probe and never
/// promises more than was proved: at each stage it carries only the proofs
/// taken so far ([`select`]).
///
/// `ask_for_grant` asks for administrator rights for the fallback alone and
/// reports whether the grant is on the machine. It is called only for
/// [`Proof::Refused`]: a probe that could not run says nothing about named
/// objects.
// Only the Windows paths call it; the host tests prove its words.
#[cfg_attr(not(windows), allow(dead_code))]
fn decide_launch_path(
    store: &Path,
    outcomes: &BTreeMap<String, StepOutcome>,
    run_probe: &mut dyn FnMut() -> Proof,
    ask_for_grant: &mut dyn FnMut() -> std::result::Result<(), String>,
    say: &mut dyn FnMut(String),
    warn: &mut dyn FnMut(String),
) -> Result<Active> {
    let mut attempt = Attempt {
        service_failed: step_failure(outcomes, &StepId::Service.key()),
        ..Attempt::default()
    };
    say(PROBE_INTRO.to_owned());
    match &attempt.service_failed {
        // Not measured: there is no launcher service to measure. The
        // selection below falls to the owner's session with no grant, which
        // promises nothing, and says why.
        Some(why) => warn(format!(
            "  not proved   kGraf's launcher service is not on this machine, so the probe was \
             not run through it: {why}"
        )),
        None => {
            let recorded = select(&attempt);
            write_active(store, &recorded)?;
            say(recorded_words(&recorded));
            let found = run_probe();
            say(probe_words(&found));
            attempt.service_probe = Some(found);
        }
    }
    if let Some(Proof::Refused(why)) = attempt.service_probe.clone() {
        warn(fallback_notice(&why));
        match ask_for_grant() {
            Ok(()) => {
                attempt.granted = true;
                let recorded = select(&attempt);
                write_active(store, &recorded)?;
                say(recorded_words(&recorded));
                let found = run_probe();
                say(probe_words(&found));
                attempt.grant_probe = Some(found);
            }
            Err(why) => {
                warn(format!("  failed       the fallback grant: {why}"));
                attempt.grant_failed = Some(why);
            }
        }
    } else if let Some(Proof::CouldNotRun(_)) = &attempt.service_probe {
        warn(COULD_NOT_RUN_NOTICE.to_owned());
    }
    let active = select(&attempt);
    write_active(store, &active)?;
    Ok(active)
}

/// Which launch path is active, and what a probe through it finds now.
/// Printed by `selfcheck` beside the sandbox's own lines, as the network and
/// DNS steps are.
///
/// A refused named object does not fail the check: on the owner-session path
/// it is a known property of that path, which these lines name, and not a
/// hole in the sandbox. The lines are printed whatever they say (fail loud).
#[cfg(windows)]
fn print_named_object_lines(backend: Backend) {
    // Only the sandbox user has a launch path. Under the token sandbox the
    // object directory refuses a restricted token as well, and the notice on
    // a failed cargo says so.
    if backend != Backend::WindowsSandboxUser {
        return;
    }
    let tag = std::env::var(probe::TAG_ENV).ok().filter(|t| !t.is_empty());
    let Some(store) = kgraf_sandbox::sandbox_user::credentials_path(tag.as_deref()) else {
        eprintln!(
            "  named objects: this user's configuration folder could not be found, so the \
             launch path is not known"
        );
        return;
    };
    println!("  {}", read_active(&store).line());
    // The record this process read at its first sandboxed command is the one
    // on disk, so the probe runs here and needs no process of its own.
    let found = match launch_probe_as_sandbox_user() {
        Ok((code, output)) => probe::proof_of_relay(code, &output),
        Err(why) => Proof::CouldNotRun(why),
    };
    println!("  {}", probe_words(&found).trim());
}

/// Where this user's sandbox passwords are kept (sealed for this user).
#[cfg(windows)]
fn credentials_path(tag: Option<&str>) -> Result<std::path::PathBuf> {
    kgraf_sandbox::sandbox_user::credentials_path(tag)
        .ok_or_else(|| anyhow::anyhow!("cannot find this user's configuration folder"))
}

#[cfg(windows)]
fn owner_names(tag: Option<&str>) -> Result<kgraf_sandbox::sandbox_user::OwnerNames> {
    use kgraf_sandbox::sandbox_user::{OwnerNames, windows::accounts};
    let sid = accounts::current_user_sid()?;
    Ok(OwnerNames::derive(&sid, tag)?)
}

#[cfg(windows)]
fn print_event(
    names: &kgraf_sandbox::sandbox_user::OwnerNames,
    e: &kgraf_sandbox::sandbox_user::handshake::Event,
) {
    use kgraf_sandbox::sandbox_user::handshake::Event;
    use kgraf_sandbox::sandbox_user::plan::StepId;
    match e {
        Event::Started => println!("The administrator step started."),
        Event::Step { key, outcome } => {
            let what = StepId::from_key(key).map_or_else(|| key.clone(), |s| s.describe(names));
            println!("  {outcome:<12} {what}");
        }
        // Left alone, with why: the run goes on, so this is not an error.
        Event::Note { key, message } => println!("  kept         {key}: {message}"),
        // Refused by Windows; setup goes on and ends with a notice.
        Event::Gap { key, message } => {
            let what = kgraf_sandbox::sandbox_user::WfpObject::from_key(key)
                .map_or_else(|| key.clone(), |o| o.noun().to_owned());
            eprintln!("  refused      {what}: {message}");
        }
        Event::Failed { key, message } => eprintln!("  failed       {key}: {message}"),
        Event::PasswordReceived(role) => println!("  received     the {role} user's password"),
        // Read past, and the run fails at the end with every name listed.
        Event::Unparsed(name) => eprintln!("  not understood {name}"),
        Event::Finished => println!("The administrator step finished."),
    }
}

#[cfg(windows)]
fn launch_exe() -> Result<String> {
    Ok(std::env::current_exe()?.to_string_lossy().into_owned())
}

/// Start the probe as a sandbox user, through the launch path the record
/// names, and return its exit status and everything it printed.
///
/// The probe is this executable with its own hidden subcommand, run like any
/// other sandboxed command: the sandbox chooses the user and the launch path,
/// so the probe measures the real path and not a special case of it. The
/// network is denied, which is the policy a build gets.
#[cfg(windows)]
fn launch_probe_as_sandbox_user() -> std::result::Result<(Option<i32>, String), String> {
    use kgraf_sandbox::{Network, Redirect, SandboxPolicy, sandboxed_command};

    let exe = std::env::current_exe().map_err(|e| format!("find this program: {e}"))?;
    let dir = tempfile::tempdir().map_err(|e| format!("make a temporary folder: {e}"))?;
    let tree = kgraf_core::plain_path::canonicalize(dir.path())
        .map_err(|e| format!("read the temporary folder's real path: {e}"))?;
    let policy = SandboxPolicy::new(vec![tree.clone()], Network::Deny);
    let mut cmd = sandboxed_command(&policy, &exe, &tree)
        .map_err(|e| format!("the sandbox refused the probe: {e}"))?;
    cmd.arg(probe::SUBCOMMAND)
        .stdout(Redirect::Piped)
        .stderr(Redirect::Piped);
    let child = cmd
        .spawn()
        .map_err(|e| format!("start the probe as a sandbox user: {e}"))?;
    let out = child
        .wait_with_output_within(probe::WAIT)
        .map_err(|e| format!("wait for the probe: {e}"))?;
    Ok((
        out.status.code(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    ))
}

/// Time the relaying process gets beyond the probe's own bound, to start,
/// report and exit.
#[cfg(windows)]
const PROBE_RELAY_MARGIN: std::time::Duration = std::time::Duration::from_secs(30);

/// Run the probe through the recorded launch path, in a process of its own.
///
/// A process of its own because the launch path is read once per process:
/// setup has just written the record, and this process read the record that
/// was there before it. The child reads what setup wrote.
#[cfg(windows)]
fn probe_in_a_new_process(exe: &str, tag: Option<&str>) -> Proof {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg(probe::SUBCOMMAND).arg(probe::LAUNCH_FLAG);
    // A tagged setup proves its own users, never the owner's.
    if let Some(tag) = tag {
        cmd.env(probe::TAG_ENV, tag);
    }
    // Bounded, under the spawn lock: the child waits on its own probe for at
    // most `probe::WAIT`, so a child still running well past that is stuck,
    // and a stuck probe proves nothing either way.
    let limits = kgraf_core::child_run::Limits::new(probe::WAIT + PROBE_RELAY_MARGIN);
    match kgraf_core::child_run::run(cmd, "the launch probe", None, limits) {
        Ok(out) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            probe::proof_of_relay(out.status.code(), &text)
        }
        Err(e) => Proof::CouldNotRun(format!("the probe did not finish: {e}")),
    }
}

/// Ask for administrator rights for the fallback grant alone, and report
/// whether the grant is on the machine.
///
/// The elevated side of [`GRANT_ACTION`] writes the grant and nothing else.
/// Whether it is there is read from the step's own outcome, never from the
/// action having been started.
#[cfg(windows)]
fn ask_for_the_fallback_grant(
    names: &kgraf_sandbox::sandbox_user::OwnerNames,
    exe: &str,
) -> std::result::Result<(), String> {
    use kgraf_sandbox::sandbox_user::windows::setup::{
        ElevatedAction, Launch, grant_named_objects,
    };

    // The word the elevated side reads must be the word this side names, or
    // the prompt would ask for administrator rights for an action that side
    // refuses. Said here rather than found out after the prompt.
    if ElevatedAction::GrantNamedObjects.key() != GRANT_ACTION {
        return Err(format!(
            "kGraf asks for the fallback with the action {GRANT_ACTION}, which the \
             administrator step does not read; the fallback is not asked for"
        ));
    }
    let mut outcomes = BTreeMap::new();
    grant_named_objects(
        names,
        &Launch {
            exe,
            subcommand: "sandbox elevated",
        },
        &mut |e| {
            record_outcome(&mut outcomes, e);
            print_event(names, e);
        },
    )
    .map_err(|e| e.to_string())?;
    match step_failure(&outcomes, &StepId::NamedObjects.key()) {
        Some(why) => Err(why),
        None => Ok(()),
    }
}

/// The hidden subcommand `probe::SUBCOMMAND`: the probe itself, or with
/// `probe::LAUNCH_FLAG` the owner's side that starts it as a sandbox user.
///
/// Both forms exit with a status a caller reads, so they end the process
/// here instead of returning an error, whose status would say nothing about
/// which of the three outcomes was found.
pub fn named_object_probe(launch: bool) -> Result<()> {
    if launch {
        #[cfg(windows)]
        match launch_probe_as_sandbox_user() {
            Ok((code, output)) => {
                print!("{output}");
                std::process::exit(code.unwrap_or(probe::FAILED_EXIT));
            }
            Err(why) => {
                eprintln!("{why}");
                std::process::exit(probe::LAUNCH_FAILED_EXIT);
            }
        }
        // Nothing to launch: there is nothing to prove on this platform, and
        // the probe below says so.
    }
    let outcome = probe::run();
    for line in &outcome.made {
        println!("{line}");
    }
    if let Some(failed) = &outcome.failed {
        eprintln!("{failed}");
    }
    if outcome.code != probe::MADE_EXIT {
        std::process::exit(outcome.code);
    }
    Ok(())
}

/// `kgraf sandbox setup`.
pub fn setup(tag: Option<&str>, slots: u8) -> Result<()> {
    #[cfg(windows)]
    {
        use kgraf_sandbox::sandbox_user::MAX_SLOTS;
        use kgraf_sandbox::sandbox_user::windows::setup::{Launch, setup};
        if slots > MAX_SLOTS {
            anyhow::bail!("--slots must be {MAX_SLOTS} or fewer");
        }
        let names = owner_names(tag)?.with_slots(slots);
        let store = credentials_path(tag)?;
        let exe = launch_exe()?;
        println!(
            "kGraf asks Windows for administrator rights once, to create its sandbox users, firewall rules and network filters."
        );
        let mut outcomes = BTreeMap::new();
        let refused = setup(
            &names,
            &Launch {
                exe: &exe,
                subcommand: "sandbox elevated",
            },
            &store,
            &mut |e| {
                record_outcome(&mut outcomes, e);
                print_event(&names, e);
            },
        )?;
        println!(
            "The Windows sandbox user is set up. Passwords are stored for this Windows user only."
        );
        if slots == 0 {
            println!(
                "No worker slots: commands without the network run as one user, so only \
                 commands in one folder run at a time, and a command in another folder is \
                 refused until they end. Commands with the network do the same. Run setup \
                 again with --slots to change that."
            );
        } else {
            println!(
                "{slots} worker slots, each with a user without the network and one with it: \
                 up to {slots} commands in different folders run at the same time, each as its \
                 own user. Another is refused until one ends."
            );
        }
        // Which launch path a command takes, measured on this machine and
        // recorded for every later command. Said out loud, step by step: a
        // sandbox that gives less than it promised is a defect.
        let active = decide_launch_path(
            &store,
            &outcomes,
            &mut || probe_in_a_new_process(&exe, tag),
            &mut || ask_for_the_fallback_grant(&names, &exe),
            &mut |line| println!("{line}"),
            &mut |line| eprintln!("{line}"),
        )?;
        println!("{}", active.line());
        // Last, so it is the line the owner reads: the gap Windows left.
        if let Some(notice) = kgraf_sandbox::sandbox_user::plan::dns_notice(&refused) {
            eprintln!("{notice}");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (tag, slots);
        println!("{}", kgraf_sandbox::sandbox_user::no_setup_needed_notice());
        Ok(())
    }
}

/// `kgraf sandbox remove`.
pub fn remove(tag: Option<&str>) -> Result<()> {
    #[cfg(windows)]
    {
        use kgraf_sandbox::sandbox_user::MAX_SLOTS;
        use kgraf_sandbox::sandbox_user::windows::setup::{Launch, remove};
        // Every possible slot, so a removal clears what a larger setup made.
        let names = owner_names(tag)?.with_slots(MAX_SLOTS);
        let store = credentials_path(tag)?;
        let exe = launch_exe()?;
        let left = remove(
            &names,
            &Launch {
                exe: &exe,
                subcommand: "sandbox elevated",
            },
            &store,
            &mut |e| print_event(&names, e),
        )?;
        println!(
            "The Windows sandbox user, its firewall rules and its network filters are removed."
        );
        if !left.is_empty() {
            // The users are gone, so these entries name a SID no account
            // has and grant nothing; they are reported so the owner can
            // clear them where the folder stays, and the command fails so a
            // removal that left one is never read as finished.
            println!(
                "{} place(s) kept an access-list entry for a sandbox user:",
                left.len()
            );
            for line in &left {
                println!("  {line}");
            }
        }
        kgraf_sandbox::sandbox_user::plan::removal_outcome(&left, None)
            .map_err(|why| anyhow::anyhow!("the removal did not finish: {why}"))
    }
    #[cfg(not(windows))]
    {
        let _ = tag;
        println!("{}", kgraf_sandbox::sandbox_user::no_setup_needed_notice());
        Ok(())
    }
}

/// `kgraf sandbox status`.
pub fn status(tag: Option<&str>) -> Result<()> {
    #[cfg(windows)]
    {
        let names = owner_names(tag)?;
        let store = credentials_path(tag)?;
        let report = kgraf_sandbox::sandbox_user::windows::setup::status(&names, &store)?;
        println!("{report}");
        // The launcher service and the fallback grant have a status line each
        // in the report above; this is the line that says what a command
        // actually gets, read from the record setup wrote.
        println!("{}", read_active(&store).line());
        if report.only_refused_gaps() {
            bail!(
                "{} not confined on this machine: Windows refused a WFP filter at setup",
                kgraf_sandbox::sandbox_user::plan::refused_subject(&report.refused_gaps())
            );
        }
        if !report.ready {
            bail!("the Windows sandbox user is not fully set up");
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = tag;
        println!("{}", kgraf_sandbox::sandbox_user::no_setup_needed_notice());
        Ok(())
    }
}

/// The hidden step `setup` and `remove` start with administrator rights.
pub fn elevated(
    action: &str,
    pipe: String,
    nonce: String,
    owner_pid: u32,
    owner_sid: String,
    tag: Option<String>,
    slots: Option<u8>,
) -> Result<()> {
    #[cfg(windows)]
    {
        use kgraf_sandbox::sandbox_user::windows::setup::{
            ElevatedAction, ElevatedArgs, elevated_main,
        };
        // Every action the elevated side knows, by its own word, so a word
        // kGraf's own process passes cannot drift from the word that side
        // reads (`GRANT_ACTION` among them).
        let Some(action) = ElevatedAction::from_key(action) else {
            bail!("unknown elevated action `{action}`");
        };
        let code = elevated_main(&ElevatedArgs {
            action,
            pipe,
            nonce,
            owner_pid,
            owner_sid,
            tag,
            slots,
        });
        if code != 0 {
            std::process::exit(code);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (action, pipe, nonce, owner_pid, owner_sid, tag, slots);
        bail!("the elevated sandbox step exists only on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Backend; 4] = [
        Backend::MacosSeatbelt,
        Backend::LinuxLandlock,
        Backend::WindowsRestrictedToken,
        Backend::WindowsSandboxUser,
    ];

    #[test]
    fn exit_codes_have_distinct_explanations() {
        let all: Vec<String> = [Some(11), Some(12), Some(13), Some(14), Some(2), None]
            .into_iter()
            .map(explain)
            .collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn probe_has_its_body_placeholder() {
        assert!(PROBE.contains("__BODY__"));
    }

    /// The sandbox user, like the Unix backends, must block a direct
    /// connection: reaching the origin directly is a failure there.
    #[test]
    fn a_direct_connection_fails_the_check_where_the_backend_confines_the_network() {
        for backend in ALL {
            if backend == Backend::WindowsRestrictedToken {
                continue;
            }
            let err = verdict(backend, Some(14)).unwrap_err();
            assert!(err.contains("direct connection"), "{backend:?}: {err}");
            assert_eq!(
                verdict(backend, Some(0)),
                Ok(NetworkResult::Confined),
                "{backend:?}"
            );
        }
    }

    /// The token sandbox cannot confine the network: a pass says so, and never
    /// reports the network as confined, whether or not the direct connection
    /// worked.
    #[test]
    fn the_token_sandbox_never_reports_a_confined_network() {
        let b = Backend::WindowsRestrictedToken;
        assert_eq!(verdict(b, Some(14)), Ok(NetworkResult::NotConfined));
        assert_eq!(verdict(b, Some(0)), Ok(NetworkResult::NotConfined));
    }

    /// A failed step fails the check on every backend, the token sandbox
    /// included.
    #[test]
    fn every_other_probe_status_fails_on_every_backend() {
        for backend in ALL {
            for code in [Some(11), Some(12), Some(13), Some(2), None] {
                assert!(verdict(backend, code).is_err(), "{backend:?} {code:?}");
            }
        }
    }

    #[test]
    fn a_pass_prints_the_backend_and_only_a_full_pass_says_ok() {
        for backend in ALL {
            let lines = report_lines(backend, NetworkResult::Confined, &DnsResult::NotChecked);
            assert_eq!(lines[0], "sandbox selfcheck: ok");
            assert!(lines.contains(&format!("  backend: {}", backend_name(backend))));
            assert!(!lines.iter().any(|l| l.contains("dns:")), "{lines:?}");
        }
        let gap = report_lines(
            Backend::WindowsRestrictedToken,
            NetworkResult::NotConfined,
            &DnsResult::OpenUnderTokenSandbox,
        );
        assert_ne!(gap[0], "sandbox selfcheck: ok");
        assert!(
            gap.contains(
                &"  network: not confined (token sandbox; run `kgraf sandbox setup`)".to_owned()
            ),
            "{gap:?}"
        );
        assert!(
            gap.contains(
                &"  dns: not confined (token sandbox; run `kgraf sandbox setup`)".to_owned()
            ),
            "{gap:?}"
        );
        assert!(!gap.iter().any(|l| l.contains("only the egress proxy")));
    }

    /// Under the sandbox user the `dns:` line passes only when the lookup was
    /// proved confined. A leak, or a run that proved nothing, prints a failed
    /// line and fails the whole check, never `ok`.
    #[test]
    fn the_sandbox_user_says_ok_only_when_dns_was_proved_confined() {
        let b = Backend::WindowsSandboxUser;
        let ok = report_lines(
            b,
            NetworkResult::Confined,
            &DnsResult::Judged(DnsVerdict::Confined),
        );
        assert_eq!(ok[0], "sandbox selfcheck: ok");
        assert!(
            ok.iter().any(|l| l.starts_with("  dns: confined")),
            "{ok:?}"
        );
        for v in [
            DnsVerdict::Leaked("the sandbox user's lookup got a DNS server's answer".into()),
            DnsVerdict::NotProved("the cache cannot tell".into()),
        ] {
            let dns = DnsResult::Judged(v);
            assert!(dns.failed());
            let lines = report_lines(b, NetworkResult::Confined, &dns);
            assert_eq!(lines[0], "sandbox selfcheck: failed", "{lines:?}");
            assert!(!lines.iter().any(|l| l.contains("dns: confined")));
        }
        let leak = report_lines(
            b,
            NetworkResult::Confined,
            &DnsResult::Judged(DnsVerdict::Leaked("x".into())),
        );
        assert!(
            leak.iter()
                .any(|l| l.starts_with("  dns: not confined (lookups leave the machine)")),
            "{leak:?}"
        );
        assert!(!DnsResult::NotChecked.failed());
        assert!(!DnsResult::OpenUnderTokenSandbox.failed());
    }

    /// Where the system allows the policy the check passes; where a Linux
    /// system denies user namespaces the error says so. Any other failure
    /// is a defect.
    #[cfg(unix)]
    #[test]
    fn passes_or_refuses_for_want_of_user_namespaces() {
        match selfcheck() {
            Ok(()) => {}
            Err(e) => {
                let text = format!("{e:#}");
                assert!(
                    cfg!(target_os = "linux") && text.contains("user namespaces"),
                    "{text}"
                );
            }
        }
    }

    /// On Windows the real probe runs through Git Bash. Without Git Bash the
    /// error names it; any other failure is a defect.
    #[cfg(windows)]
    #[test]
    fn passes_or_names_the_missing_git_bash() {
        if let Err(e) = selfcheck() {
            let text = format!("{e:#}");
            assert!(text.contains("Git"), "{text}");
        }
    }
}

/// The launch path setup decides and records (decision 0097). Every test
/// here runs on every host: the decision, the record and the words are
/// platform-neutral, and the only Windows parts are the two closures
/// `decide_launch_path` is given.
#[cfg(test)]
mod launch_path_tests {
    use super::*;
    use kgraf_sandbox::sandbox_user::session0::LaunchPath;

    /// A temporary store, never this user's real configuration folder.
    fn store(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("sandbox-credentials.bin")
    }

    fn outcomes(pairs: &[(&str, StepOutcome)]) -> BTreeMap<String, StepOutcome> {
        let mut out = BTreeMap::new();
        for (key, outcome) in pairs {
            record_outcome(
                &mut out,
                &Event::Step {
                    key: (*key).to_owned(),
                    outcome: *outcome,
                },
            );
        }
        out
    }

    fn service_done() -> BTreeMap<String, StepOutcome> {
        outcomes(&[(&StepId::Service.key(), StepOutcome::Created)])
    }

    /// What `decide_launch_path` recorded and said in one run.
    struct Run {
        active: Active,
        /// The record on disk when each probe started, in order.
        before_each_probe: Vec<Active>,
        /// How often administrator rights were asked for.
        asked: usize,
        /// Every line the run said, the warnings first.
        said: Vec<String>,
    }

    /// Run the decision with probes that answer `proofs` in order, and a
    /// grant that answers `grant`.
    fn run(
        store: &Path,
        outcomes: &BTreeMap<String, StepOutcome>,
        proofs: &[Proof],
        grant: std::result::Result<(), String>,
    ) -> Run {
        let mut before = Vec::new();
        let mut asked = 0usize;
        // One list in the order the lines were said, warnings among them, so
        // a test can hold the notice before the prompt it belongs to. A
        // shared cell because both the plain lines and the warnings go into
        // it.
        let said = std::cell::RefCell::new(Vec::new());
        let active = {
            let mut next = proofs.iter().cloned();
            let mut run_probe = || {
                // Read the record the probe would take, as the probe's own
                // process does.
                before.push(read_active(store));
                next.next().expect("one probe more than the test allowed")
            };
            let mut ask = || {
                asked += 1;
                grant.clone()
            };
            decide_launch_path(
                store,
                outcomes,
                &mut run_probe,
                &mut ask,
                &mut |line| said.borrow_mut().push(line),
                &mut |line| said.borrow_mut().push(line),
            )
            .expect("the decision")
        };
        Run {
            active,
            before_each_probe: before,
            asked,
            said: said.into_inner(),
        }
    }

    /// The record on disk, read the way every later command reads it.
    fn recorded(store: &Path) -> Active {
        read_active(store)
    }

    /// A step the administrator run reported as done is no failure; one it
    /// did not report, or reported as part done, is.
    #[test]
    fn the_attempt_reads_the_outcome_of_each_new_step() {
        let service = StepId::Service.key();
        let named = StepId::NamedObjects.key();
        for done in [
            StepOutcome::Created,
            StepOutcome::Updated,
            StepOutcome::Verified,
        ] {
            let map = outcomes(&[(&service, done)]);
            assert_eq!(step_failure(&map, &service), None, "{done}");
        }
        // Nothing reported: kGraf cannot say the step is on the machine.
        let missing = step_failure(&BTreeMap::new(), &service).expect("a reason");
        assert!(missing.contains(&service), "{missing}");
        assert!(missing.contains("did not report"), "{missing}");
        for half in [
            StepOutcome::Partial,
            StepOutcome::Removed,
            StepOutcome::AlreadyGone,
        ] {
            let map = outcomes(&[(&named, half)]);
            let why = step_failure(&map, &named).expect("a reason");
            assert!(why.contains(&named), "{half}: {why}");
        }
        // One step's outcome says nothing about the other.
        let map = outcomes(&[(&service, StepOutcome::Created)]);
        assert!(step_failure(&map, &named).is_some());
    }

    /// The service path is written before it is measured, so the probe takes
    /// it, and the record carries no proof until one was taken. A proof then
    /// makes it the active path, and nothing asks for administrator rights.
    #[test]
    fn the_service_path_is_recorded_unproved_then_proved() {
        let dir = tempfile::tempdir().expect("a temporary folder");
        let store = store(&dir);
        let out = run(&store, &service_done(), &[Proof::Made], Ok(()));
        assert_eq!(out.asked, 0, "{:?}", out.said);
        assert_eq!(out.before_each_probe.len(), 1);
        let before = &out.before_each_probe[0];
        assert_eq!(before.path, LaunchPath::Session0);
        assert_eq!(before.proved, None);
        assert!(
            !before.cargo_runs(),
            "a record written before a proof must promise nothing"
        );
        assert_eq!(out.active, Active::session0_proved());
        assert_eq!(recorded(&store), Active::session0_proved());
        assert!(recorded(&store).cargo_runs());
        assert!(
            out.said.iter().any(|l| l.contains("proved")),
            "{:?}",
            out.said
        );
    }

    /// A refused named object is what the fallback exists for: the owner is
    /// told the cost before the prompt, administrator rights are asked for
    /// once, the grant's path is recorded before it is measured, and the
    /// proof through it is what makes cargo run.
    #[test]
    fn a_refused_object_asks_once_for_the_fallback_and_proves_it() {
        let dir = tempfile::tempdir().expect("a temporary folder");
        let store = store(&dir);
        let out = run(
            &store,
            &service_done(),
            &[
                Proof::Refused("exit 1, Access is denied".into()),
                Proof::Made,
            ],
            Ok(()),
        );
        assert_eq!(out.asked, 1, "{:?}", out.said);
        assert_eq!(out.before_each_probe.len(), 2);
        let second = &out.before_each_probe[1];
        assert_eq!(second.path, LaunchPath::OwnerSession);
        assert!(second.granted);
        assert_eq!(second.proved, None, "no proof before one was taken");
        let active = recorded(&store);
        assert_eq!(active, out.active);
        assert_eq!(active.path, LaunchPath::OwnerSession);
        assert!(active.granted);
        assert!(active.cargo_runs());
        assert!(active.because.contains("its proof failed"), "{active:?}");
        // The cost is said before the prompt, not after it.
        let notice = out
            .said
            .iter()
            .position(|l| l.contains("administrator rights once more"))
            .expect("the notice");
        let granted = out
            .said
            .iter()
            .position(|l| l.contains("owner-session"))
            .expect("the recorded line");
        assert!(notice < granted, "{:?}", out.said);
    }

    /// The words before the second prompt name what failed, that more rights
    /// are needed, what the fallback allows, what it costs and which rights
    /// it adds.
    #[test]
    fn the_message_before_the_second_elevation_says_the_cost() {
        let notice = fallback_notice("exit 1, Access is denied");
        for part in [
            "could not make the named kernel objects",
            "exit 1, Access is denied",
            "administrator rights once more",
            "the owner's own programs read",
            "DIRECTORY_CREATE_OBJECT",
            "cargo and rustc cannot run as a sandbox user",
        ] {
            assert!(notice.contains(part), "{part}: {notice}");
        }
        assert!(notice.contains(named_object_grant::EXPLANATION), "{notice}");
    }

    /// A probe that could not run says nothing about named objects. It must
    /// not ask for administrator rights, and must leave the service path
    /// active and unproved.
    #[test]
    fn a_probe_that_could_not_run_asks_for_no_elevation() {
        let dir = tempfile::tempdir().expect("a temporary folder");
        let store = store(&dir);
        let out = run(
            &store,
            &service_done(),
            &[Proof::CouldNotRun("the probe would not start".into())],
            Ok(()),
        );
        assert_eq!(out.asked, 0, "{:?}", out.said);
        let active = recorded(&store);
        assert_eq!(active.path, LaunchPath::Session0);
        assert!(!active.granted);
        assert!(!active.cargo_runs(), "{active:?}");
        assert_eq!(
            active.proved,
            Some(Proof::CouldNotRun("the probe would not start".into()))
        );
        assert!(
            out.said
                .iter()
                .any(|l| l.contains("does not ask for administrator rights")),
            "{:?}",
            out.said
        );
    }

    /// A launch that never started the probe is read from its own exit
    /// status, so its reason is never mistaken for a refused named object
    /// even where Windows words the two the same way.
    #[test]
    fn a_launch_that_never_ran_is_not_a_refused_object() {
        let denial = "start the probe as a sandbox user: Access is denied. (os error 5)";
        assert_eq!(
            probe::proof_of_relay(Some(probe::LAUNCH_FAILED_EXIT), denial),
            Proof::CouldNotRun(denial.to_owned())
        );
        // The probe's own refusal still reads as one.
        let refused = probe::failed_line(
            probe::Object::Mutex,
            "kgraf-probe-mutex-00ff",
            "Access is denied. (os error 5) (Windows error 5)",
        );
        assert!(matches!(
            probe::proof_of_relay(Some(probe::FAILED_EXIT), &refused),
            Proof::Refused(_)
        ));
        assert_eq!(
            probe::proof_of_relay(Some(0), "made the named semaphore x"),
            Proof::Made
        );
    }

    /// A fallback that could not be written leaves the owner's session with
    /// no grant: the path that promises nothing, with both reasons in it.
    #[test]
    fn a_failed_grant_is_recorded_and_said() {
        let dir = tempfile::tempdir().expect("a temporary folder");
        let store = store(&dir);
        let out = run(
            &store,
            &service_done(),
            &[Proof::Refused("exit 1, Access is denied".into())],
            Err("the elevated step was refused".into()),
        );
        assert_eq!(out.asked, 1);
        let active = recorded(&store);
        assert_eq!(active.path, LaunchPath::OwnerSession);
        assert!(!active.granted);
        assert!(!active.cargo_runs());
        assert!(active.because.contains("its proof failed"), "{active:?}");
        assert!(
            active.because.contains("the elevated step was refused"),
            "{active:?}"
        );
        assert!(
            out.said
                .iter()
                .any(|l| l.contains("the elevated step was refused")),
            "{:?}",
            out.said
        );
    }

    /// Without a launcher service there is nothing to measure through it:
    /// no probe runs, nothing is asked for, and the record promises nothing.
    #[test]
    fn a_missing_service_step_is_not_probed() {
        let dir = tempfile::tempdir().expect("a temporary folder");
        let store = store(&dir);
        let out = run(&store, &BTreeMap::new(), &[], Ok(()));
        assert_eq!(out.asked, 0);
        assert!(out.before_each_probe.is_empty());
        let active = recorded(&store);
        assert!(!active.cargo_runs(), "{active:?}");
        assert!(active.because.contains("could not be set up"), "{active:?}");
    }

    /// A record that is not there reads as the path that promises least, the
    /// same way a sandboxed command's own launch reads it. One that was
    /// written comes back as it was written.
    #[test]
    fn a_missing_record_reads_as_the_least_promising_path() {
        let dir = tempfile::tempdir().expect("a temporary folder");
        let store = store(&dir);
        let active = read_active(&store);
        assert_eq!(active.path, LaunchPath::OwnerSession);
        assert!(!active.granted);
        assert_eq!(active.proved, None);
        assert!(!active.cargo_runs());
        write_active(&store, &Active::session0_proved()).expect("write the record");
        assert_eq!(read_active(&store), Active::session0_proved());
    }

    /// The fallback's own elevated action, by the word the elevated side
    /// reads, with the parameters that action needs and no others: it makes
    /// no user, so it is never told a slot count.
    #[test]
    fn the_grant_action_carries_the_owners_names_and_no_slot_count() {
        use kgraf_sandbox::sandbox_user::OwnerNames;
        use kgraf_sandbox::sandbox_user::plan::elevated_params;

        assert_eq!(GRANT_ACTION, "grant-named-objects");
        let names = OwnerNames::derive("S-1-5-21-1-2-3-1001", Some("t1")).expect("names");
        let params = elevated_params(&names, GRANT_ACTION, r"\\.\pipe\p", "00ff", 4321);
        for part in [
            "--action grant-named-objects",
            r"--pipe \\.\pipe\p",
            "--nonce 00ff",
            "--owner-pid 4321",
            "--owner-sid S-1-5-21-1-2-3-1001",
            "--tag t1",
        ] {
            assert!(params.contains(part), "{part}: {params}");
        }
        assert!(!params.contains("--slots"), "{params}");
    }

    /// Each line says which stage it is about, so a reader follows the order
    /// setup took.
    #[test]
    fn each_line_names_its_stage() {
        assert!(
            PROBE_INTRO.contains("named kernel objects"),
            "{PROBE_INTRO}"
        );
        let recorded = recorded_words(&Active::session0_proved());
        assert!(recorded.contains("session-0"), "{recorded}");
        assert!(recorded.contains("not yet proved"), "{recorded}");
        assert!(probe_words(&Proof::Made).contains("proved"));
        let refused = probe_words(&Proof::Refused("denied".into()));
        assert!(refused.contains("refused"), "{refused}");
        assert!(refused.contains("denied"), "{refused}");
        let not_run = probe_words(&Proof::CouldNotRun("no start".into()));
        assert!(
            not_run.contains("says nothing about named objects"),
            "{not_run}"
        );
    }
}
