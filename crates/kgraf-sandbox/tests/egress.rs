//! The network a dependency install gets (decision 0087, amended
//! 2026-10-07): only kGraf's egress proxy, which reaches only the allowed
//! hosts; and the folder an install may not touch inside the tree it writes
//! (`SandboxPolicy::hide`). Against the real backend: Seatbelt on macOS, the
//! network and mount namespaces on Linux.
//!
//! What it shows: through the proxy a command reaches an allowed origin and
//! is refused, with the host named, for any other; without the proxy it
//! reaches nothing, not even another loopback port (a direct connection
//! bypassing the proxy fails in the sandbox); a name lookup fails. A hidden
//! folder can neither be read, written, renamed nor replaced, while a
//! writable root inside it stays writable and the tree around it writable.
//!
//! How to run: `cargo test -p kgraf-sandbox --test egress` on macOS or Linux.
//! On Linux the backend needs user namespaces; where they are not available
//! (a default container seccomp profile), the two sandbox tests print why and
//! return, since kGraf refuses such an install loudly.
//! No network: every origin is a test listener on the loopback; `curl` is
//! the client (in macOS and in the Linux test image).

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kgraf_sandbox::egress::{EgressProxy, HostRule};
use kgraf_sandbox::{SandboxPolicy, sandboxed_command};

/// A loopback origin answering each request with `body`, counting requests.
fn origin(body: &'static str) -> (u16, Arc<Mutex<usize>>) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let port = l.local_addr().expect("addr").port();
    let hits = Arc::new(Mutex::new(0usize));
    let h = hits.clone();
    std::thread::spawn(move || {
        for c in l.incoming() {
            let Ok(mut c) = c else { continue };
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf);
            *h.lock().expect("lock") += 1;
            let _ = write!(
                c,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (port, hits)
}

fn canonical(dir: &tempfile::TempDir) -> PathBuf {
    std::fs::canonicalize(dir.path()).expect("canonical")
}

/// Run `script` with `sh -c` in `cwd` under `policy`, started through
/// `proxy` with its variables set.
fn sh(proxy: &EgressProxy, policy: &SandboxPolicy, cwd: &Path, script: &str) -> (bool, String) {
    let mut cmd = sandboxed_command(policy, "/bin/sh", cwd).expect("command");
    for (k, v) in proxy.env() {
        cmd.env(k, v);
    }
    cmd.args(["-c", script])
        .stdout(kgraf_sandbox::Redirect::Piped)
        .stderr(kgraf_sandbox::Redirect::Piped);
    let child = proxy.spawn(&mut cmd).expect("spawn");
    let out = child.wait_with_output().expect("wait");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

/// False, with the reason printed, where the backend cannot enforce `policy`
/// for want of user namespaces (a container under a default seccomp profile,
/// a kernel or distribution that restricts them). kGraf refuses such an
/// install loudly; there is nothing of the sandbox to test. Any other refusal
/// fails the test.
fn enforceable(policy: &SandboxPolicy, cwd: &Path) -> bool {
    match sandboxed_command(policy, "/bin/sh", cwd) {
        Ok(_) => true,
        Err(kgraf_sandbox::SandboxError::PolicyNotEnforceable { what, .. })
            if cfg!(target_os = "linux") && what.contains("user namespaces are not available") =>
        {
            eprintln!("skipped: {what}");
            false
        }
        Err(e) => panic!("command: {e:?}"),
    }
}

#[test]
fn only_allowed_hosts_are_reached_and_only_through_the_proxy() {
    let (allowed, allowed_hits) = origin("from-the-registry");
    let (other, other_hits) = origin("from-elsewhere");
    let proxy = EgressProxy::start(vec![
        HostRule::parse(&format!("127.0.0.1:{allowed}")).expect("rule"),
    ])
    .expect("proxy");
    let tree = tempfile::tempdir().expect("tree");
    let t = canonical(&tree);
    let policy = SandboxPolicy::new(vec![t.clone()], proxy.network());
    if !enforceable(&policy, &t) {
        return;
    }

    let (ok, out) = sh(
        &proxy,
        &policy,
        &t,
        &format!("curl -sS --max-time 10 http://127.0.0.1:{allowed}/pkg"),
    );
    assert!(ok && out.contains("from-the-registry"), "allowed: {out}");
    assert_eq!(*allowed_hits.lock().expect("lock"), 1);

    let (ok, out) = sh(
        &proxy,
        &policy,
        &t,
        &format!("curl -sS --max-time 10 -f http://127.0.0.1:{other}/steal"),
    );
    assert!(!ok, "a host off the list must fail: {out}");
    assert!(out.contains("403"), "{out}");

    // Straight to the other port, with no proxy: the sandbox refuses it.
    let (ok, out) = sh(
        &proxy,
        &policy,
        &t,
        &format!("curl -sS --max-time 10 --noproxy '*' http://127.0.0.1:{other}/direct"),
    );
    assert!(!ok, "a direct connection must fail: {out}");
    // And to the allowed origin directly too: only the proxy's port is open.
    let (ok, out) = sh(
        &proxy,
        &policy,
        &t,
        &format!("curl -sS --max-time 10 --noproxy '*' http://127.0.0.1:{allowed}/direct"),
    );
    assert!(
        !ok,
        "even an allowed host is reached only through the proxy: {out}"
    );
    // A name lookup is a way out too; it must not resolve.
    let (ok, out) = sh(
        &proxy,
        &policy,
        &t,
        "curl -sS --max-time 10 --noproxy '*' http://example.com/",
    );
    assert!(!ok, "no name lookup: {out}");

    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(*other_hits.lock().expect("lock"), 0, "never contacted");
    assert_eq!(
        *allowed_hits.lock().expect("lock"),
        1,
        "the direct try never arrived"
    );
    assert_eq!(proxy.blocked(), vec![format!("127.0.0.1:{other}")]);
    assert_eq!(proxy.contacted(), vec![format!("127.0.0.1:{allowed}")]);
}

#[test]
fn a_hidden_folder_is_untouchable_while_a_root_inside_it_stays_writable() {
    let checkout = tempfile::tempdir().expect("checkout");
    let c = canonical(&checkout);
    let state = c.join(".kgraf");
    let store = state.join("package-stores");
    std::fs::create_dir_all(&store).expect("store");
    std::fs::write(state.join("dependency-fetch.json"), "{\"secret\": 1}").expect("write");
    std::fs::create_dir_all(c.join(".git/hooks")).expect("git");
    let proxy = EgressProxy::start(Vec::new()).expect("proxy");
    let policy =
        SandboxPolicy::new(vec![c.clone(), store.clone()], proxy.network()).hide(state.clone());
    if !enforceable(&policy, &c) {
        return;
    }

    let (ok, out) = sh(&proxy, &policy, &c, "echo tree > in-tree.txt");
    assert!(ok, "the tree is writable: {out}");
    let (ok, out) = sh(
        &proxy,
        &policy,
        &c,
        "echo cached > .kgraf/package-stores/cache",
    );
    assert!(ok, "the store is writable: {out}");
    assert_eq!(
        std::fs::read_to_string(store.join("cache")).expect("store file"),
        "cached\n"
    );
    for (what, script) in [
        ("read", "cat .kgraf/dependency-fetch.json"),
        ("list", "ls .kgraf | grep dependency-fetch"),
        ("write", "echo x > .kgraf/dependency-fetch.json"),
        ("create", "echo x > .kgraf/new.json"),
        ("delete", "rm .kgraf/dependency-fetch.json"),
        ("rename", "mv .kgraf moved"),
        (
            "replace",
            "rm -rf .kgraf; mkdir .kgraf && echo x > .kgraf/planted",
        ),
        ("hook", "echo x > .git/hooks/pre-commit"),
    ] {
        let (ok, out) = sh(&proxy, &policy, &c, script);
        assert!(!ok, "{what} must fail: {out}");
    }
    assert_eq!(
        std::fs::read_to_string(state.join("dependency-fetch.json")).expect("state"),
        "{\"secret\": 1}"
    );
    assert!(!state.join("new.json").exists());
    assert!(!state.join("planted").exists());
    assert!(!c.join("moved").exists());
    assert!(!c.join(".git/hooks/pre-commit").exists());
    assert_eq!(
        std::fs::read_to_string(c.join("in-tree.txt")).expect("tree"),
        "tree\n"
    );
}

/// A hidden folder must lie beneath a writable root.
#[test]
fn a_hidden_folder_outside_every_root_is_refused() {
    let root = tempfile::tempdir().expect("root");
    let other = tempfile::tempdir().expect("other");
    let proxy = EgressProxy::start(Vec::new()).expect("proxy");
    let policy =
        SandboxPolicy::new(vec![canonical(&root)], proxy.network()).hide(canonical(&other));
    let err = sandboxed_command(&policy, "/bin/sh", &canonical(&root)).expect_err("refused");
    assert!(
        err.to_string().contains("not beneath a writable root"),
        "{err}"
    );
}
