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

// ---- the Windows sandbox user (decision 0097) -------------------------------

/// Where this user's sandbox passwords are kept (sealed for this user).
#[cfg(windows)]
fn credentials_path(tag: Option<&str>) -> Result<std::path::PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", kgraf_diag::paths::APP_NAME)
        .ok_or_else(|| anyhow::anyhow!("cannot find this user's configuration folder"))?;
    let file = match tag {
        Some(t) => format!("sandbox-credentials-{t}.bin"),
        None => "sandbox-credentials.bin".to_owned(),
    };
    Ok(dirs.config_dir().join(file))
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
        Event::Failed { key, message } => eprintln!("  failed       {key}: {message}"),
        Event::PasswordReceived(role) => println!("  received     the {role} user's password"),
        Event::Finished => println!("The administrator step finished."),
    }
}

#[cfg(windows)]
fn launch_exe() -> Result<String> {
    Ok(std::env::current_exe()?.to_string_lossy().into_owned())
}

/// `kgraf sandbox setup`.
pub fn setup(tag: Option<&str>) -> Result<()> {
    #[cfg(windows)]
    {
        use kgraf_sandbox::sandbox_user::windows::setup::{Launch, setup};
        let names = owner_names(tag)?;
        let store = credentials_path(tag)?;
        let exe = launch_exe()?;
        println!(
            "kGraf asks Windows for administrator rights once, to create its sandbox users and firewall rules."
        );
        setup(
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
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = tag;
        println!("{}", kgraf_sandbox::sandbox_user::no_setup_needed_notice());
        Ok(())
    }
}

/// `kgraf sandbox remove`.
pub fn remove(tag: Option<&str>) -> Result<()> {
    #[cfg(windows)]
    {
        use kgraf_sandbox::sandbox_user::windows::setup::{Launch, remove};
        let names = owner_names(tag)?;
        let store = credentials_path(tag)?;
        let exe = launch_exe()?;
        remove(
            &names,
            &Launch {
                exe: &exe,
                subcommand: "sandbox elevated",
            },
            &store,
            &mut |e| print_event(&names, e),
        )?;
        println!("The Windows sandbox user and its firewall rules are removed.");
        Ok(())
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
        });
        if code != 0 {
            std::process::exit(code);
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = (action, pipe, nonce, owner_pid, owner_sid, tag);
        bail!("the elevated sandbox step exists only on Windows")
    }
}
