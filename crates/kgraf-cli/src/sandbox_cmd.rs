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
        let refused = setup(
            &names,
            &Launch {
                exe: &exe,
                subcommand: "sandbox elevated",
            },
            &store,
            &mut |e| print_event(&names, e),
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
        let action = match action {
            "setup" => ElevatedAction::Setup,
            "remove" => ElevatedAction::Remove,
            other => bail!("unknown elevated action `{other}`"),
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
