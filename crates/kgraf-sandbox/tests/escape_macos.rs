//! Escape attempts against the real macOS Seatbelt backend (acceptance: "verified by an
//! escape attempt test on each supported platform"). Each denied operation is run on its own and must fail with
//! EPERM ("Operation not permitted"); the filesystem is checked afterward.
//! No test reads a real credential: sockets are fake listeners, mach lookups
//! only resolve a service name, and credential-path reads use a fake HOME
//! (see `reads_macos.rs`).

#![cfg(target_os = "macos")]

mod common;

use std::net::TcpListener;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

use common::fixture;
use kgraf_sandbox::{
    Network, SandboxPolicy, Workspace, WorkspaceManager, WorkspaceSpec, sandboxed_command,
    workspace_command,
};

const EPERM_TEXT: &str = "Operation not permitted";

struct Env {
    f: common::Fixture,
    _mgr: WorkspaceManager,
    ws: Workspace,
    outside: tempfile::TempDir,
}

fn env() -> Env {
    let f = fixture();
    let mgr = WorkspaceManager::open(&f.repo, &f.base).unwrap();
    let ws = mgr
        .create(&WorkspaceSpec {
            name: "w",
            base: "HEAD",
            branch: None,
        })
        .unwrap();
    let outside = tempfile::tempdir().unwrap();
    Env {
        f,
        _mgr: mgr,
        ws,
        outside,
    }
}

fn sh(ws: &Workspace, network: Network, script: &str) -> Output {
    workspace_command(ws, network, "/bin/sh")
        .unwrap()
        .arg("-c")
        .arg(script)
        .output()
        .unwrap()
}

/// Run one operation; it must fail, and fail with EPERM.
fn assert_eperm(ws: &Workspace, script: &str) {
    let out = sh(ws, Network::Deny, script);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{script}: succeeded: {out:?}");
    assert!(
        stderr.contains(EPERM_TEXT),
        "{script}: wrong failure: {stderr}"
    );
}

fn q(p: &Path) -> String {
    format!("'{}'", p.display())
}

/// A small C probe, built once outside the sandbox. It prints only numbers.
fn probe() -> &'static Path {
    static PROBE: OnceLock<PathBuf> = OnceLock::new();
    PROBE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let src = dir.join("probe.c");
        std::fs::write(
            &src,
            r#"
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <servers/bootstrap.h>
int main(int argc, char **argv) {
  if (argc < 2) return 2;
  if (!strcmp(argv[1], "mach")) {
    mach_port_t port;
    kern_return_t kr = bootstrap_look_up(bootstrap_port, argv[2], &port);
    printf("%d\n", kr);
    return kr == KERN_SUCCESS ? 0 : 1;
  }
  if (!strcmp(argv[1], "unix")) {
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s < 0) { printf("socket %d\n", errno); return 1; }
    struct sockaddr_un a;
    memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    strncpy(a.sun_path, argv[2], sizeof(a.sun_path) - 1);
    if (connect(s, (struct sockaddr *)&a, sizeof a) != 0) { printf("%d\n", errno); return 1; }
    printf("0\n");
    return 0;
  }
  if (!strcmp(argv[1], "session")) {
    int fd = open("/dev/tty", O_RDWR);
    printf("leader=%d tty_errno=%d\n", getsid(0) == getpid(), fd < 0 ? errno : 0);
    return 0;
  }
  return 2;
}
"#,
        )
        .unwrap();
        let bin = dir.join("probe");
        let status = Command::new("cc")
            .arg(&src)
            .arg("-o")
            .arg(&bin)
            .status()
            .unwrap();
        assert!(status.success(), "probe build failed");
        bin
    })
}

fn run_probe(policy: &SandboxPolicy, cwd: &Path, args: &[&str]) -> (bool, String) {
    let out = sandboxed_command(policy, probe(), cwd)
        .unwrap()
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

#[test]
fn write_inside_the_workspace_succeeds() {
    let e = env();
    let target = e.ws.root().join("src/new.rs");
    let out = sh(
        &e.ws,
        Network::Deny,
        &format!("mkdir -p src && echo inside > {}", q(&target)),
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "inside\n");
}

#[test]
fn write_outside_the_workspace_fails_with_eperm() {
    let e = env();
    let target = e.outside.path().join("escaped");
    assert_eperm(&e.ws, &format!("echo out > {}", q(&target)));
    assert!(!target.exists());
}

#[test]
fn writes_to_the_workspace_parent_and_main_repo_fail_with_eperm() {
    let e = env();
    let sibling = e.ws.root().parent().unwrap().join("sibling");
    let repo_file = e.f.repo.join("README");
    assert_eperm(&e.ws, &format!("echo x > {}", q(&sibling)));
    assert_eperm(&e.ws, &format!("echo tamper >> {}", q(&repo_file)));
    assert!(!sibling.exists());
    assert_eq!(std::fs::read_to_string(&repo_file).unwrap(), "hello\n");
}

#[test]
fn modifying_or_deleting_an_existing_outside_file_fails_with_eperm() {
    let e = env();
    let victim = e.outside.path().join("victim");
    std::fs::write(&victim, "original").unwrap();
    let v = q(&victim);
    assert_eperm(&e.ws, &format!("echo x >> {v}"));
    assert_eperm(&e.ws, &format!(": > {v}"));
    assert_eperm(&e.ws, &format!("rm -f {v}"));
    assert_eperm(&e.ws, &format!("mv {v} {v}.moved"));
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
}

#[test]
fn symlink_inside_pointing_outside_does_not_escape() {
    let e = env();
    let out = sh(
        &e.ws,
        Network::Deny,
        &format!("ln -s {} link", q(e.outside.path())),
    );
    assert!(
        out.status.success(),
        "creating a symlink inside is allowed: {out:?}"
    );
    assert!(e.ws.root().join("link").is_symlink());
    assert_eperm(&e.ws, "echo x > link/via-symlink");
    assert!(!e.outside.path().join("via-symlink").exists());
}

#[test]
fn hardlink_to_an_outside_file_is_refused() {
    let e = env();
    let victim = e.outside.path().join("victim");
    std::fs::write(&victim, "original").unwrap();
    assert_eperm(&e.ws, &format!("ln {} hard", q(&victim)));
    let hard = e.ws.root().join("hard");
    if let Ok(meta) = std::fs::symlink_metadata(&hard) {
        let v = std::fs::metadata(&victim).unwrap();
        assert!(
            (meta.dev(), meta.ino()) != (v.dev(), v.ino()),
            "a hard link to the victim was created"
        );
    }
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original");
}

#[test]
fn child_processes_inherit_the_confinement() {
    let e = env();
    let target = e.outside.path().join("grandchild");
    assert_eperm(
        &e.ws,
        &format!("/bin/sh -c \"/usr/bin/touch {}\"", target.display()),
    );
    assert!(!target.exists());
}

#[test]
fn workspace_root_cannot_be_renamed() {
    let e = env();
    let root = e.ws.root();
    assert_eperm(&e.ws, &format!("cd / && mv {r} {r}.moved", r = q(root)));
    assert!(root.is_dir());
}

#[test]
fn gitlink_cannot_be_rewritten_or_removed_from_inside() {
    let e = env();
    assert_eperm(&e.ws, "echo 'gitdir: /tmp/evil' > .git");
    assert_eperm(&e.ws, "rm -f .git");
    e.ws.verify_gitlink().unwrap();
}

#[test]
fn tmpdir_is_the_scratch_directory_and_is_writable() {
    let e = env();
    let out = sh(
        &e.ws,
        Network::Deny,
        "echo t > \"$TMPDIR/t\" && printf %s \"$TMPDIR\"",
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        Path::new(&*String::from_utf8_lossy(&out.stdout)),
        e.ws.scratch()
    );
    assert!(e.ws.scratch().join("t").is_file());
}

/// The go command writes its build cache on every build and test and stops
/// when it cannot; its default under `~/Library/Caches` is outside the
/// writable roots, so the sandbox points `GOCACHE` into the scratch directory.
#[test]
fn the_go_build_cache_is_in_scratch_and_writable() {
    let e = env();
    let out = sh(
        &e.ws,
        Network::Deny,
        "mkdir -p \"$GOCACHE\" && echo t > \"$GOCACHE/t\" && printf %s \"$GOCACHE\"",
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        Path::new(&*String::from_utf8_lossy(&out.stdout)),
        e.ws.scratch().join("go-build")
    );
    assert!(e.ws.scratch().join("go-build/t").is_file());
}

#[test]
fn a_real_cc_compile_runs_inside_the_sandbox() {
    let e = env();
    std::fs::write(
        e.ws.root().join("hello.c"),
        "#include <stdio.h>\nint main(void){puts(\"built\");return 0;}\n",
    )
    .unwrap();
    let out = sh(&e.ws, Network::Deny, "cc hello.c -o hello && ./hello");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "built\n");
}

#[test]
fn a_real_cargo_build_runs_inside_the_sandbox() {
    let e = env();
    let root = e.ws.root();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src/main.rs"),
        "fn main() { println!(\"cargo built\"); }\n",
    )
    .unwrap();
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = workspace_command(&e.ws, Network::Deny, &cargo)
        .unwrap()
        .args(["run", "--offline", "-q"])
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "cargo built\n");
}

/// Needs internet. The unsandboxed control runs first, so an offline host
/// fails with "control failed", not with a misleading sandbox failure.
#[test]
fn https_certificate_validation_works_with_network_allowed() {
    let e = env();
    let curl = "/usr/bin/curl -sS -o /dev/null -w '%{http_code}' https://crates.io";
    // crates.io answers curl's default user agent with 403; any real status
    // code proves the TLS handshake and certificate validation succeeded.
    // A validation failure is exit 60 with code 000.
    let host = Command::new("/bin/sh").args(["-c", curl]).output().unwrap();
    let host_code = String::from_utf8_lossy(&host.stdout).to_string();
    assert!(
        host.status.success() && host_code != "000",
        "control failed (offline?): {host:?}"
    );
    let out = sh(&e.ws, Network::Allow, curl);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), host_code, "{out:?}");
    // And a 200 from the crates.io index host, same TLS path.
    let index = sh(
        &e.ws,
        Network::Allow,
        "/usr/bin/curl -sS -o /dev/null -w '%{http_code}' https://index.crates.io/config.json",
    );
    assert_eq!(String::from_utf8_lossy(&index.stdout), "200", "{index:?}");
    // Validation is really on inside the sandbox: a certificate the system
    // does not trust is rejected (curl exit 60), not waved through. The
    // server is local, so no third-party host is involved.
    let server = UntrustedTlsServer::start(e.outside.path());
    let url = format!("https://localhost:{}", server.port);
    let curl = format!("/usr/bin/curl -sS -o /dev/null {CURL_VALIDATION_FLAGS} {url}");
    let control = Command::new("/bin/sh")
        .args(["-c", &curl])
        .output()
        .unwrap();
    assert_eq!(control.status.code(), Some(60), "control: {control:?}");
    let rejected = sh(&e.ws, Network::Allow, &curl);
    assert_eq!(rejected.status.code(), Some(60), "{rejected:?}");
}

/// Extra curl flags for the local rejection check. Empty keeps validation on.
const CURL_VALIDATION_FLAGS: &str = "";

/// A local TLS server on 127.0.0.1 with a self-signed certificate for
/// `localhost` that no trust store holds. The name matches, so the only
/// reason curl can reject it is trust. The child is killed on drop.
struct UntrustedTlsServer {
    child: std::process::Child,
    port: u16,
}

impl UntrustedTlsServer {
    fn start(dir: &Path) -> Self {
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        let made = Command::new("/usr/bin/openssl")
            .args([
                "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            ])
            .args(["-subj", "/CN=localhost"])
            .args(["-addext", "subjectAltName=DNS:localhost"])
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .unwrap();
        assert!(made.status.success(), "openssl req failed: {made:?}");
        // Pick a free port, release it, then let the server take it.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let child = Command::new("/usr/bin/openssl")
            .args(["s_server", "-www", "-accept"])
            .arg(port.to_string())
            .arg("-cert")
            .arg(&cert)
            .arg("-key")
            .arg(&key)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let server = Self { child, port };
        let up = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        assert!(up, "local TLS server did not start");
        server
    }
}

impl Drop for UntrustedTlsServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Needs internet: a fresh CARGO_HOME inside the workspace scratch fetches
/// the crates.io index and one crate over HTTPS.
#[test]
fn cargo_fetch_of_one_dependency_works_with_network_allowed() {
    let e = env();
    let root = e.ws.root();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [dependencies]\nitoa = \"=1.0.15\"\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    let cargo_home = e.ws.scratch().join("cargo-home");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = workspace_command(&e.ws, Network::Allow, &cargo)
        .unwrap()
        .args(["fetch", "--quiet"])
        .env("CARGO_HOME", &cargo_home)
        .env_remove("CARGO_TARGET_DIR")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        std::fs::read_dir(cargo_home.join("registry/cache"))
            .unwrap()
            .flatten()
            .any(|d| std::fs::read_dir(d.path())
                .unwrap()
                .flatten()
                .any(|f| f.file_name().to_string_lossy().starts_with("itoa-1.0.15"))),
        "itoa was not downloaded"
    );
}

/// Needs internet and npm. ~/.npmrc is read-denied; npm must still work
/// because the sandbox points NPM_CONFIG_USERCONFIG at an empty scratch file.
#[test]
fn npm_works_with_npmrc_denied() {
    let e = env();
    let Some(npm) = ["/opt/homebrew/bin/npm", "/usr/local/bin/npm"]
        .into_iter()
        .find(|p| Path::new(p).is_file())
    else {
        panic!("npm not installed; this test needs it");
    };
    let config = sh(
        &e.ws,
        Network::Allow,
        &format!("{npm} config get userconfig"),
    );
    assert!(config.status.success(), "{config:?}");
    assert_eq!(
        Path::new(String::from_utf8_lossy(&config.stdout).trim()),
        e.ws.scratch().join(".npmrc")
    );
    let view = sh(
        &e.ws,
        Network::Allow,
        &format!("{npm} view left-pad@1.3.0 version"),
    );
    assert!(view.status.success(), "{view:?}");
    assert_eq!(String::from_utf8_lossy(&view.stdout).trim(), "1.3.0");
}

#[test]
fn network_deny_blocks_tcp_and_allow_permits_it() {
    let e = env();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let probe = format!("/usr/bin/nc -z -w 2 127.0.0.1 {port}");
    let denied = sh(&e.ws, Network::Deny, &probe);
    assert!(
        !denied.status.success(),
        "connect allowed under Deny: {denied:?}"
    );
    let allowed = sh(&e.ws, Network::Allow, &probe);
    assert!(
        allowed.status.success(),
        "connect blocked under Allow: {allowed:?}"
    );
}

#[test]
fn outside_unix_socket_is_unreachable_under_both_network_modes() {
    let e = env();
    // Stands in for $SSH_AUTH_SOCK, D-Bus or docker.sock.
    let path = e.outside.path().join("agent.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let sock = path.to_str().unwrap();
    for network in [Network::Deny, Network::Allow] {
        let policy = SandboxPolicy::for_workspace(&e.ws, network);
        let (ok, errno) = run_probe(&policy, e.ws.root(), &["unix", sock]);
        assert!(!ok, "{network:?}: connected to an outside socket");
        assert_eq!(errno, libc::EPERM.to_string(), "{network:?}");
    }
    assert!(
        listener.accept().is_err(),
        "the fake agent accepted a connection"
    );

    // Controls: the probe does connect when the socket is permitted.
    let allowed =
        SandboxPolicy::for_workspace(&e.ws, Network::Deny).allow_unix_socket(path.clone());
    assert_eq!(
        run_probe(&allowed, e.ws.root(), &["unix", sock]),
        (true, "0".into())
    );
    let inner = e.ws.scratch().join("inner.sock");
    let _inner = UnixListener::bind(&inner).unwrap();
    let policy = SandboxPolicy::for_workspace(&e.ws, Network::Deny);
    assert_eq!(
        run_probe(&policy, e.ws.root(), &["unix", inner.to_str().unwrap()]),
        (true, "0".into())
    );
}

#[test]
fn keychain_security_server_is_unreachable() {
    let e = env();
    const SERVICE: &str = "com.apple.SecurityServer";
    // Control: the probe resolves it outside the sandbox.
    let host = Command::new(probe())
        .args(["mach", SERVICE])
        .output()
        .unwrap();
    assert!(host.status.success(), "control lookup failed: {host:?}");
    for network in [Network::Deny, Network::Allow] {
        let policy = SandboxPolicy::for_workspace(&e.ws, network);
        let (ok, kr) = run_probe(&policy, e.ws.root(), &["mach", SERVICE]);
        assert!(!ok, "{network:?}: SecurityServer reachable (kr {kr})");
        // Control: an allowed service still resolves inside the sandbox.
        let (ok, kr) = run_probe(
            &policy,
            e.ws.root(),
            &["mach", "com.apple.system.opendirectoryd.libinfo"],
        );
        assert!(ok, "{network:?}: allowed lookup failed (kr {kr})");
    }
}

#[test]
fn child_runs_in_a_new_session_without_a_terminal() {
    let e = env();
    let policy = SandboxPolicy::for_workspace(&e.ws, Network::Deny);
    let (ok, out) = run_probe(&policy, e.ws.root(), &["session"]);
    assert!(ok);
    // Session leader of a fresh session: no controlling terminal to inject
    // into. Opening /dev/tty fails: ENXIO (no ctty) or EPERM (the profile
    // also withholds /dev/tty).
    let tty_errno = out.strip_prefix("leader=1 tty_errno=").unwrap_or("");
    assert!(
        [libc::ENXIO, libc::EPERM]
            .map(|e| e.to_string())
            .contains(&tty_errno.to_string()),
        "{out}"
    );
    // Control: without the sandbox the probe is not a session leader.
    let host = Command::new(probe()).arg("session").output().unwrap();
    assert!(
        String::from_utf8_lossy(&host.stdout).starts_with("leader=0"),
        "{host:?}"
    );
}

#[test]
fn explicit_policy_confines_to_its_roots() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::new(vec![root.path().to_path_buf()], Network::Deny);
    let out = sandboxed_command(&policy, "/bin/sh", root.path())
        .unwrap()
        .arg("-c")
        .arg(format!(
            "echo a > in && echo b > {}",
            q(&other.path().join("out"))
        ))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains(EPERM_TEXT));
    assert!(root.path().join("in").is_file());
    assert!(!other.path().join("out").exists());
}
