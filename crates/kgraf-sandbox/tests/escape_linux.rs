//! Escape attempts against the real Linux backend (Landlock + seccomp +
//! namespaces; acceptance: "verified by an escape attempt
//! test on each supported platform"). Each denied operation runs on its own
//! and must fail; the host side is checked afterward. No test touches a real
//! credential: sockets are test listeners, signal targets are test
//! processes, and docker.sock (when present) only sees a connect attempt,
//! never a request.
//!
//! Tests that depend on optional kernel features (namespaces, Landlock ABI 6)
//! assert the strength report instead of skipping: where a feature is
//! missing, the gap it would close must be real and reported.

#![cfg(target_os = "linux")]

mod common;

use std::net::TcpListener;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::OnceLock;

use common::fixture;
use kgraf_sandbox::{
    Network, SandboxPolicy, Workspace, WorkspaceManager, WorkspaceSpec, linux_confinement,
    platform_isolation, sandboxed_command, workspace_command,
};

const EPERM: &str = "1";

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

/// Run one operation; it must fail.
fn assert_fails(ws: &Workspace, script: &str) -> String {
    let out = sh(ws, Network::Deny, script);
    assert!(!out.status.success(), "{script}: succeeded: {out:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn q(p: &Path) -> String {
    format!("'{}'", p.display())
}

/// A small C probe, built once outside the sandbox. It prints the errno of
/// the first failing call, or 0.
fn probe() -> &'static Path {
    static PROBE: OnceLock<PathBuf> = OnceLock::new();
    PROBE.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let src = dir.join("probe.c");
        std::fs::write(
            &src,
            r#"
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <sys/ioctl.h>
#include <sys/ptrace.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <linux/netlink.h>
#include <sched.h>
#include <sys/mount.h>
#define FAIL(what) do { printf("%d\n", errno); return 1; } while (0)
static int un(const char *path, int abstract) {
  int s = socket(AF_UNIX, SOCK_STREAM, 0);
  if (s < 0) FAIL("socket");
  struct sockaddr_un a;
  memset(&a, 0, sizeof a);
  a.sun_family = AF_UNIX;
  socklen_t len = sizeof a;
  if (abstract) {
    strncpy(a.sun_path + 1, path, sizeof(a.sun_path) - 2);
    len = offsetof(struct sockaddr_un, sun_path) + 1 + strlen(path);
  } else {
    strncpy(a.sun_path, path, sizeof(a.sun_path) - 1);
  }
  if (connect(s, (struct sockaddr *)&a, len) != 0) FAIL("connect");
  printf("0\n");
  return 0;
}
int main(int argc, char **argv) {
  if (argc < 2) return 2;
  const char *op = argv[1];
  if (!strcmp(op, "unix")) return un(argv[2], 0);
  if (!strcmp(op, "abstract")) return un(argv[2], 1);
  if (!strcmp(op, "tcp") || !strcmp(op, "udp")) {
    int s = socket(AF_INET, !strcmp(op, "tcp") ? SOCK_STREAM : SOCK_DGRAM, 0);
    if (s < 0) FAIL("socket");
    struct sockaddr_in a;
    memset(&a, 0, sizeof a);
    a.sin_family = AF_INET;
    a.sin_port = htons(atoi(argv[2]));
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    if (connect(s, (struct sockaddr *)&a, sizeof a) != 0) FAIL("connect");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "netlink")) {
    if (socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE) < 0) FAIL("socket");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "socketpair")) {
    int sv[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) != 0) FAIL("socketpair");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "ptrace")) {
    if (ptrace(PTRACE_ATTACH, atoi(argv[2]), 0, 0) != 0) FAIL("ptrace");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "vmread")) {
    char buf[8], *remote = (char *)0x10000;
    struct iovec l = { buf, sizeof buf }, r = { remote, sizeof buf };
    if (process_vm_readv(atoi(argv[2]), &l, 1, &r, 1, 0) < 0) FAIL("vmread");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "kill")) {
    if (kill(atoi(argv[2]), SIGTERM) != 0) FAIL("kill");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "io_uring")) {
    char params[120];
    memset(params, 0, sizeof params);
    if (syscall(SYS_io_uring_setup, 1, params) < 0) FAIL("io_uring");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "keyctl")) {
    /* KEYCTL_GET_KEYRING_ID (0) of the session keyring (-3). */
    if (syscall(SYS_keyctl, 0, -3, 0) < 0) FAIL("keyctl");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "tiocsti")) {
    char c = 'x';
    if (ioctl(0, TIOCSTI, &c) != 0) FAIL("tiocsti");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "unshare")) {
    if (unshare(CLONE_NEWUSER) != 0) FAIL("unshare");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "clonens")) {
    long pid = syscall(SYS_clone, CLONE_NEWUSER | SIGCHLD, 0, 0, 0, 0);
    if (pid < 0) FAIL("clone");
    if (pid == 0) _exit(0);
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "clone3")) {
    if (syscall(SYS_clone3, 0, 0) < 0) FAIL("clone3");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "mount")) {
    if (mount("none", "/tmp", "tmpfs", 0, 0) != 0) FAIL("mount");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "writefd")) {
    if (write(atoi(argv[2]), "escaped\n", 8) != 8) FAIL("write");
    printf("0\n");
    return 0;
  }
#if defined(__x86_64__)
  if (!strcmp(op, "x32socket")) {
    if (syscall(0x40000000 + 41, AF_UNIX, SOCK_STREAM, 0) < 0) FAIL("x32");
    printf("0\n");
    return 0;
  }
  if (!strcmp(op, "int80")) {
    long ret;
    /* i386 getpid (20) through the compat entry point. */
    __asm__ volatile ("int $0x80" : "=a"(ret) : "a"(20) : "memory");
    printf("%ld\n", ret);
    return 0;
  }
#endif
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

/// (success, stdout) of the probe under `network`, in the workspace.
fn run_probe(ws: &Workspace, network: Network, args: &[&str]) -> (bool, String) {
    run_probe_with(probe_policy(ws, network), ws.root(), args)
}

/// Reads are an allowlist on Linux: the probe's directory must be on it.
fn probe_policy(ws: &Workspace, network: Network) -> SandboxPolicy {
    SandboxPolicy::for_workspace(ws, network)
        .with_readable_root(probe().parent().unwrap().to_path_buf())
}

fn run_probe_with(policy: SandboxPolicy, cwd: &Path, args: &[&str]) -> (bool, String) {
    let out = sandboxed_command(&policy, probe(), cwd)
        .unwrap()
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

/// Control: the probe does the operation when run unconfined.
fn host_probe(args: &[&str]) -> (bool, String) {
    let out = Command::new(probe()).args(args).output().unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

/// A process outside the sandbox, killed on drop.
struct Victim(Child);

impl Victim {
    fn spawn() -> Self {
        Self(Command::new("sleep").arg("60").spawn().unwrap())
    }
    fn pid(&self) -> String {
        self.0.id().to_string()
    }
    fn alive(&mut self) -> bool {
        self.0.try_wait().unwrap().is_none()
    }
}

impl Drop for Victim {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn the_strength_report_matches_the_kernel() {
    let k = linux_confinement();
    let s = platform_isolation().unwrap();
    eprintln!("kernel: {k:?}\nstrength: {s:?}");
    let full = k.seccomp && k.landlock_abi >= 3;
    assert_eq!(s.writes_confined, full);
    assert_eq!(s.unix_sockets_confined, full);
    assert_eq!(s.network_deniable, full);
    assert_eq!(s.gitlink_protected, full && k.namespaces);
    assert_eq!(s.gaps.contains("signals"), !k.signals_scoped() || !full);
    // The development and CI kernels this suite runs on have both.
    assert!(full, "this kernel lacks Landlock ABI 3 or seccomp: {k:?}");
}

#[test]
fn a_default_policy_runs_without_the_partial_isolation_opt_in() {
    let e = env();
    let out = sh(&e.ws, Network::Deny, "echo ok");
    assert!(out.status.success(), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "ok\n");
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
fn writes_outside_the_workspace_fail() {
    let e = env();
    let target = e.outside.path().join("escaped");
    let sibling = e.ws.root().parent().unwrap().join("sibling");
    let repo_file = e.f.repo.join("README");
    let victim = e.outside.path().join("victim");
    std::fs::write(&victim, "original").unwrap();
    let v = q(&victim);
    for script in [
        format!("echo out > {}", q(&target)),
        format!("echo x > {}", q(&sibling)),
        format!("echo tamper >> {}", q(&repo_file)),
        format!("echo x >> {v}"),
        format!(": > {v}"),
        format!("rm -f {v}"),
        format!("mv {v} {v}.moved"),
        format!("ln {v} hard"),
        format!("/bin/sh -c \"/usr/bin/touch {}\"", target.display()),
    ] {
        assert_fails(&e.ws, &script);
    }
    assert!(!target.exists() && !sibling.exists());
    assert_eq!(std::fs::read_to_string(&repo_file).unwrap(), "hello\n");
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
    assert!(out.status.success(), "{out:?}");
    assert_fails(&e.ws, "echo x > link/via-symlink");
    assert!(!e.outside.path().join("via-symlink").exists());
}

#[test]
fn workspace_root_cannot_be_renamed() {
    let e = env();
    let root = e.ws.root();
    assert_fails(&e.ws, &format!("cd / && mv {r} {r}.moved", r = q(root)));
    assert!(root.is_dir());
}

#[test]
fn gitlink_is_protected_exactly_when_reported() {
    let e = env();
    let protected = platform_isolation().unwrap().gitlink_protected;
    let rewrite = sh(&e.ws, Network::Deny, "echo 'gitdir: /tmp/evil' > .git");
    let remove = sh(&e.ws, Network::Deny, "rm -f .git");
    if protected {
        assert!(!rewrite.status.success(), "{rewrite:?}");
        assert!(
            String::from_utf8_lossy(&rewrite.stderr).contains("Read-only file system"),
            "{rewrite:?}"
        );
        assert!(!remove.status.success(), "{remove:?}");
        e.ws.verify_gitlink().unwrap();
    } else {
        // The reported gap is real: the worker can rewrite its gitlink, and
        // the host must call verify_gitlink before git operations.
        assert!(rewrite.status.success(), "{rewrite:?}");
        assert!(e.ws.verify_gitlink().is_err());
    }
}

#[test]
fn outside_unix_socket_is_unreachable_under_both_network_modes() {
    let e = env();
    // Stands in for $SSH_AUTH_SOCK or D-Bus.
    let path = e.outside.path().join("agent.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let sock = path.to_str().unwrap();
    assert_eq!(host_probe(&["unix", sock]), (true, "0".into()), "control");
    let _ = listener.accept().unwrap();
    for network in [Network::Deny, Network::Allow] {
        assert_eq!(
            run_probe(&e.ws, network, &["unix", sock]),
            (false, EPERM.into()),
            "{network:?}"
        );
    }
    assert!(
        listener.accept().is_err(),
        "the fake agent accepted a connection"
    );
    // Stricter than macOS: a socket inside the workspace is refused too,
    // and so is an allowlist entry (refused before anything runs).
    let inner = e.ws.scratch().join("inner.sock");
    let _inner = UnixListener::bind(&inner).unwrap();
    assert_eq!(
        run_probe(&e.ws, Network::Deny, &["unix", inner.to_str().unwrap()]),
        (false, EPERM.into())
    );
    let allowed = probe_policy(&e.ws, Network::Deny).allow_unix_socket(path);
    assert!(sandboxed_command(&allowed, probe(), e.ws.root()).is_err());
}

#[test]
fn abstract_unix_socket_is_unreachable() {
    let e = env();
    let name = format!("kgraf-test-{}", std::process::id());
    let addr = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let listener = UnixListener::bind_addr(&addr).unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        host_probe(&["abstract", &name]),
        (true, "0".into()),
        "control"
    );
    let _ = listener.accept().unwrap();
    assert_eq!(
        run_probe(&e.ws, Network::Allow, &["abstract", &name]),
        (false, EPERM.into())
    );
    assert!(listener.accept().is_err());
}

#[test]
fn docker_socket_is_unreachable_when_present() {
    let e = env();
    let docker = Path::new("/var/run/docker.sock");
    if !docker.exists() {
        eprintln!("no {} on this host; nothing to reach", docker.display());
        return;
    }
    // Only connect(); no request is ever sent to the daemon.
    for network in [Network::Deny, Network::Allow] {
        assert_eq!(
            run_probe(&e.ws, network, &["unix", docker.to_str().unwrap()]),
            (false, EPERM.into()),
            "{network:?}"
        );
    }
}

#[test]
fn socketpair_still_works_for_build_tools() {
    let e = env();
    assert_eq!(
        run_probe(&e.ws, Network::Deny, &["socketpair"]),
        (true, "0".into())
    );
}

#[test]
fn network_deny_blocks_tcp_udp_and_allow_permits_ip_only() {
    let e = env();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    assert_eq!(
        run_probe(&e.ws, Network::Deny, &["tcp", &port]),
        (false, EPERM.into())
    );
    assert_eq!(
        run_probe(&e.ws, Network::Deny, &["udp", &port]),
        (false, EPERM.into())
    );
    assert_eq!(
        run_probe(&e.ws, Network::Allow, &["tcp", &port]),
        (true, "0".into())
    );
    // Allowed network is IP only: no netlink, no unix.
    assert_eq!(host_probe(&["netlink"]), (true, "0".into()), "control");
    assert_eq!(
        run_probe(&e.ws, Network::Allow, &["netlink"]),
        (false, EPERM.into())
    );
}

#[test]
fn ptrace_and_process_memory_of_an_outside_process_are_refused() {
    let e = env();
    let victim = Victim::spawn();
    let pid = victim.pid();
    for op in ["ptrace", "vmread"] {
        let (ok, errno) = run_probe(&e.ws, Network::Deny, &[op, &pid]);
        assert!(!ok, "{op} succeeded");
        // EPERM from seccomp; ESRCH if the PID namespace hides it first
        // (the filter answers before the PID is looked up, so EPERM).
        assert_eq!(errno, EPERM, "{op}");
    }
    let mem = sh(&e.ws, Network::Deny, &format!("head -c 1 /proc/{pid}/mem"));
    assert!(!mem.status.success(), "{mem:?}");
}

#[test]
fn signals_to_an_outside_process_are_refused_exactly_when_reported() {
    let e = env();
    let mut victim = Victim::spawn();
    let (ok, errno) = run_probe(&e.ws, Network::Deny, &["kill", &victim.pid()]);
    std::thread::sleep(std::time::Duration::from_millis(100));
    if linux_confinement().signals_scoped() {
        assert!(!ok, "kill succeeded");
        // ESRCH: the PID does not exist in the sandbox's PID namespace.
        // EPERM: Landlock's signal scope.
        assert!(
            errno == libc::ESRCH.to_string() || errno == EPERM,
            "{errno}"
        );
        assert!(victim.alive(), "the outside process was killed");
    } else {
        // The reported gap is real.
        assert!(ok, "kill failed without any signal scoping: {errno}");
        assert!(!victim.alive());
    }
}

#[test]
fn io_uring_keyring_and_tiocsti_are_refused() {
    let e = env();
    for op in ["io_uring", "keyctl", "tiocsti"] {
        assert_eq!(
            run_probe(&e.ws, Network::Allow, &[op]),
            (false, EPERM.into()),
            "{op}"
        );
    }
}

#[test]
fn the_command_is_pid_1_of_its_namespace_when_namespaces_exist() {
    let e = env();
    let out = sh(&e.ws, Network::Deny, "echo $$");
    assert!(out.status.success(), "{out:?}");
    let pid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(pid == "1", linux_confinement().namespaces, "pid {pid}");
}

#[test]
fn exit_codes_pass_through_and_killing_the_child_ends_the_command() {
    let e = env();
    let out = sh(&e.ws, Network::Deny, "exit 7");
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    // The caller's `Child` is the PID-namespace waiter when namespaces are
    // on; killing it must still end the command it runs.
    let marker = e.ws.root().join("survived");
    let mut child = workspace_command(&e.ws, Network::Deny, "/bin/sh")
        .unwrap()
        .arg("-c")
        .arg(format!("sleep 2; touch {}", q(&marker)))
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(!marker.exists(), "the command outlived the killed child");
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
/// when it cannot; its default under `~/.cache` is readable only, so the
/// sandbox points `GOCACHE` into the scratch directory.
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
fn git_status_and_a_real_cc_compile_run_inside_the_sandbox() {
    let e = env();
    std::fs::write(
        e.ws.root().join("hello.c"),
        "#include <stdio.h>\nint main(void){puts(\"built\");return 0;}\n",
    )
    .unwrap();
    let out = sh(
        &e.ws,
        Network::Deny,
        "git status --short >/dev/null && cc hello.c -o hello && ./hello",
    );
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

/// Needs internet. DNS and TLS must work with only AF_INET/AF_INET6 sockets
/// (no netlink, no nscd or resolved unix socket).
#[test]
fn https_works_with_network_allowed() {
    let e = env();
    let curl = "curl -sS -o /dev/null -w '%{http_code}' https://index.crates.io/config.json";
    let host = Command::new("/bin/sh").args(["-c", curl]).output().unwrap();
    assert!(host.status.success(), "control failed (offline?): {host:?}");
    let out = sh(&e.ws, Network::Allow, curl);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "200", "{out:?}");
    let denied = sh(&e.ws, Network::Deny, curl);
    assert!(!denied.status.success(), "{denied:?}");
}

#[test]
fn nested_namespaces_cannot_be_created() {
    let e = env();
    for op in ["unshare", "clonens"] {
        assert_eq!(
            run_probe(&e.ws, Network::Allow, &[op]),
            (false, EPERM.into()),
            "{op}"
        );
    }
    // clone3 flags live in memory the filter cannot read: ENOSYS makes libc
    // fall back to clone, whose flags it can.
    assert_eq!(
        run_probe(&e.ws, Network::Allow, &["clone3"]),
        (false, libc::ENOSYS.to_string())
    );
    // Ordinary process creation still works (fork, pipelines).
    let out = sh(&e.ws, Network::Deny, "echo a | cat && (exit 0)");
    assert!(out.status.success(), "{out:?}");
}

#[test]
fn a_mount_attempt_is_refused() {
    let e = env();
    assert_eq!(
        run_probe(&e.ws, Network::Deny, &["mount"]),
        (false, EPERM.into())
    );
}

#[test]
fn an_inherited_descriptor_to_an_outside_file_is_closed_at_exec() {
    use std::os::fd::AsRawFd;
    let e = env();
    let victim = e.outside.path().join("victim");
    std::fs::write(&victim, "original").unwrap();
    let file = std::fs::OpenOptions::new()
        .append(true)
        .open(&victim)
        .unwrap();
    // F_DUPFD, unlike std's opens, leaves the copy without FD_CLOEXEC: the
    // leak a careless parent (or a library) would cause. A high number keeps
    // it clear of descriptors other tests use.
    // SAFETY: duplicates a descriptor this test owns; closed below.
    let leaked = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 900) };
    assert!(leaked >= 900);
    let n = leaked.to_string();
    let host = host_probe(&["writefd", &n]);
    let sandboxed = run_probe(&e.ws, Network::Deny, &["writefd", &n]);
    // SAFETY: closes the duplicate made above.
    unsafe { libc::close(leaked) };
    assert_eq!(host, (true, "0".into()), "control: the leak is real");
    assert_eq!(sandboxed, (false, libc::EBADF.to_string()));
    assert_eq!(
        std::fs::read_to_string(&victim).unwrap(),
        "originalescaped\n",
        "only the unconfined control wrote"
    );
}

#[test]
fn host_processes_are_hidden_or_guarded_in_proc() {
    let e = env();
    let victim = Victim::spawn();
    let pid = victim.pid();
    let root = sh(&e.ws, Network::Deny, &format!("ls /proc/{pid}/root/"));
    assert!(!root.status.success(), "{root:?}");
    let cmdline = sh(&e.ws, Network::Deny, &format!("cat /proc/{pid}/cmdline"));
    let k = linux_confinement();
    if k.namespaces && k.fresh_proc {
        assert!(!cmdline.status.success(), "{cmdline:?}");
        assert!(
            String::from_utf8_lossy(&cmdline.stderr).contains("No such file"),
            "{cmdline:?}"
        );
    } else {
        // The reported gap is real.
        assert!(platform_isolation().unwrap().gaps.contains("/proc lists"));
        assert!(cmdline.status.success(), "{cmdline:?}");
    }
}

#[test]
fn the_command_runs_without_capabilities() {
    let e = env();
    let out = sh(&e.ws, Network::Deny, "grep ^Cap /proc/self/status");
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .map(|v| v.trim().to_string())
            .unwrap()
    };
    for name in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
        assert_eq!(field(name), "0000000000000000", "{name}\n{text}");
    }
    // The bounding set is emptied wherever the process may (root, or root
    // of its own user namespace); elsewhere no_new_privs keeps it unused.
    // SAFETY: getuid cannot fail.
    if unsafe { libc::getuid() } == 0 || linux_confinement().namespaces {
        assert_eq!(field("CapBnd:"), "0000000000000000", "{text}");
    }
}

/// x86_64 only: the x32 ABI shares the x86_64 audit arch, so only the
/// inserted guard stops `socket(AF_UNIX)` issued with the x32 bit.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_x32_socket_call_is_refused() {
    let e = env();
    assert_eq!(
        run_probe(&e.ws, Network::Allow, &["x32socket"]),
        (false, EPERM.into())
    );
}

/// x86_64 only: an i386 syscall through `int 0x80` fails the filter's
/// architecture check, which kills the process with SIGSYS.
#[cfg(target_arch = "x86_64")]
#[test]
fn an_i386_syscall_kills_the_process() {
    use std::os::unix::process::ExitStatusExt;
    let e = env();
    let out = sandboxed_command(&probe_policy(&e.ws, Network::Deny), probe(), e.ws.root())
        .unwrap()
        .arg("int80")
        .output()
        .unwrap();
    // The PID-namespace waiter re-raises the signal it saw.
    assert_eq!(out.status.signal(), Some(libc::SIGSYS), "{out:?}");
}
