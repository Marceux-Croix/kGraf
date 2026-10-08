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

use anyhow::{Result, bail};

/// The marker the local origin answers with.
#[cfg(unix)]
const ORIGIN_BODY: &str = "selfcheck-ok";

/// The probe, run with `sh -c`. Arguments: tree, hidden folder, origin port.
/// Exit codes tell the failing step: 11 tree not writable, 12 hidden folder
/// writable, 13 proxy did not reach the origin, 14 a direct connection
/// reached the origin.
#[cfg(unix)]
const PROBE: &str = r#"
tree="$1"; hidden="$2"; port="$3"
echo inside > "$tree/selfcheck-write" || exit 11
if ( echo x > "$hidden/selfcheck-write" ) 2>/dev/null; then exit 12; fi
curl -sS --max-time 10 "http://127.0.0.1:$port/" | grep -q '__BODY__' || exit 13
if curl -sS --max-time 5 --noproxy '*' "http://127.0.0.1:$port/" >/dev/null 2>&1; then exit 14; fi
exit 0
"#;

/// What each probe exit code means.
#[cfg(unix)]
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

/// Run the check. Prints one line per step and returns an error when the
/// sandbox refuses the policy or a step fails.
#[cfg(unix)]
pub fn selfcheck() -> Result<()> {
    use anyhow::Context;
    use kgraf_sandbox::egress::{EgressProxy, HostRule};
    use kgraf_sandbox::{Redirect, SandboxPolicy, sandboxed_command};
    use std::io::{Read, Write};
    use std::net::{Ipv4Addr, TcpListener};

    let origin = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).context("bind the local origin")?;
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
    let policy = SandboxPolicy::new(vec![tree.clone()], proxy.network()).hide(hidden.clone());

    let mut cmd = sandboxed_command(&policy, "/bin/sh", &tree)
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
    let child = proxy
        .spawn(&mut cmd)
        .context("start the sandboxed command")?;
    let out = child.wait_with_output().context("wait for the command")?;
    if out.status.success() {
        println!("sandbox selfcheck: ok");
        println!("  network: only the egress proxy was reachable");
        println!("  hidden folder: not writable");
        println!("  tree: writable");
        return Ok(());
    }
    bail!(
        "sandbox selfcheck failed: {}\n{}{}",
        explain(out.status.code()),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Not available off Unix: the Windows sandbox cannot confine the network.
#[cfg(not(unix))]
pub fn selfcheck() -> Result<()> {
    bail!("sandbox selfcheck needs the proxy-limited sandbox, which this platform does not have")
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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

    /// Where the system allows the policy the check passes; where a Linux
    /// system denies user namespaces the error says so. Any other failure
    /// is a defect.
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
}
