//! kGraf's egress proxy: a small HTTP proxy that lets a sandboxed command
//! reach only an allowlist of hosts (decision 0087, amended 2026-10-07).
//!
//! The sandbox can allow the whole network or none of it; neither Seatbelt
//! nor Landlock nor a network namespace knows a host name. So a command
//! that may reach some hosts only gets [`Network::Proxy`]: one loopback
//! port, where this proxy listens, and nothing else. The command's package
//! manager is pointed at the proxy ([`EgressProxy::env`]), and the proxy
//! decides by name:
//!
//! ```text
//!   client --CONNECT host:443--> proxy --allowed?--yes--> resolve; refuse a
//!                                   |                     loopback, link-local,
//!                                   |                     metadata address --> tunnel
//!                                   `--no--> 403, the host is recorded as blocked
//!   client --GET http://host/p--> the same check, then the request is sent
//!                                 to the host in origin form, `Connection: close`
//! ```
//!
//! Every blocked host is kept ([`EgressProxy::blocked`]), so a failed command
//! can say which host it needed and how to allow it.
//!
//! **What it does not stop.** A registry accepts uploads as well as
//! downloads: a command holding its own registry token can publish to an
//! allowed registry. A server that hosts several names at one address (a
//! CDN) can be asked for another name inside an allowed tunnel. Plain HTTP
//! requests after the first on one connection go to the same allowed host.
//!
//! **Reuse.** The design follows Anthropic's sandbox-runtime
//! (github.com/anthropics/sandbox-runtime, Apache-2.0), read 2026-10-07: a
//! host-side HTTP proxy, the sandbox limited to the proxy's loopback port
//! (its Seatbelt rule form is used in the macOS backend), host patterns of
//! an exact name or `*.domain`, and a check of the address an allowed name
//! resolves to (`src/sandbox/resolved-address-guard.ts`, whose denied address
//! classes are taken here). Its code is TypeScript on Node's HTTP stack, so
//! this is a port of the behavior, not of the code (`third_party/sandbox-runtime/UPSTREAM`).
//! Searched crates.io for a
//! Rust proxy to reuse: the CONNECT proxies there (`hudsucker`,
//! `http-mitm-proxy`) intercept TLS and pull in hyper and rustls; an
//! allowlist check before a byte copy is smaller than either, and blocking
//! I/O on threads fits an install that runs for minutes with a few
//! connections.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Network;

/// The port the proxy listens on inside a Linux command's own network
/// namespace. Any port works there: nothing else is in that namespace.
#[cfg(target_os = "linux")]
pub const NAMESPACE_PORT: u16 = 3128;

/// The most bytes of a request head the proxy reads.
const MAX_HEAD: usize = 64 * 1024;
/// How long a connection may sit idle before the proxy closes it.
const IDLE: Duration = Duration::from_secs(300);
/// How long the proxy waits for an upstream connection.
const CONNECT_WAIT: Duration = Duration::from_secs(30);

/// One allowlist entry: `host`, `*.domain`, or either with `:port`.
///
/// Without a port an entry allows ports 443 and 80, the ones registries
/// serve on; any other port must be named (`127.0.0.1:4873`). `*.domain`
/// matches every name below `domain`, never `domain` itself and never an IP
/// address. An IP address entry matches only that address, spelled as an
/// address.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct HostRule {
    /// Lowercase, no trailing dot, no IPv6 brackets.
    host: String,
    port: Option<u16>,
}

impl HostRule {
    /// Parse one entry. An empty entry, a scheme, a path, a bare `*` or a
    /// wildcard anywhere but the start is refused, naming the entry.
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim();
        let bad = |why: &str| Err(format!("`{text}` is not an allowed host entry: {why}"));
        if t.is_empty() {
            return bad("it is empty");
        }
        if t.contains("://") || t.contains('/') {
            return bad("give a host name, not a URL");
        }
        let (host, port) = split_port(t);
        let port = match port {
            None => None,
            Some(p) => match p.parse::<u16>() {
                Ok(n) if n > 0 => Some(n),
                _ => return bad("the port is not a number from 1 to 65535"),
            },
        };
        let host = canonical_host(host);
        if host.is_empty() {
            return bad("it has no host");
        }
        if host == "*" {
            return bad("`*` would allow every host");
        }
        let name = host.strip_prefix("*.").unwrap_or(&host);
        if name.contains('*') {
            return bad("a wildcard is allowed only as a leading `*.`");
        }
        if host.starts_with("*.") && name.parse::<IpAddr>().is_ok() {
            return bad("a wildcard cannot cover an IP address");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_'))
        {
            return bad("it holds a character no host name has");
        }
        Ok(Self { host, port })
    }

    /// Whether this entry allows `host:port`. `host` is canonicalized first.
    pub fn allows(&self, host: &str, port: u16) -> bool {
        let ports_ok = match self.port {
            Some(p) => p == port,
            None => port == 443 || port == 80,
        };
        if !ports_ok {
            return false;
        }
        let host = canonical_host(host);
        match self.host.strip_prefix("*.") {
            Some(domain) => {
                host.parse::<IpAddr>().is_err() && host.ends_with(&format!(".{domain}"))
            }
            None => host == self.host,
        }
    }

    /// Whether this entry names an IP address (an explicit choice, never
    /// re-judged by the resolved-address check).
    fn is_address(&self) -> bool {
        self.host.parse::<IpAddr>().is_ok()
    }
}

impl std::fmt::Display for HostRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(p) => write!(f, "{host}:{p}"),
            None => f.write_str(&host),
        }
    }
}

/// `host[:port]` or `[v6][:port]`: the host and the port text, if any. An
/// unbracketed text with two or more colons is an IPv6 address with no port.
fn split_port(t: &str) -> (&str, Option<&str>) {
    if let Some(rest) = t.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((host, tail)) => (host, tail.strip_prefix(':')),
            None => (t, None),
        };
    }
    match t.matches(':').count() {
        1 => {
            let (h, p) = t.split_once(':').unwrap_or((t, ""));
            (h, Some(p))
        }
        _ => (t, None),
    }
}

/// Lowercase, without IPv6 brackets or a trailing dot.
fn canonical_host(host: &str) -> String {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Addresses an allowed host name may not resolve to, unless the allowlist
/// names that address itself: loopback, unspecified, link-local, multicast,
/// broadcast, and cloud instance-metadata endpoints outside link-local.
/// Taken from sandbox-runtime's `resolved-address-guard.ts` (see the module
/// documentation). Private ranges stay allowed: a company registry may live
/// there.
fn denied_address(ip: IpAddr) -> Option<&'static str> {
    let ip = match ip {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            if v4.is_loopback() {
                Some("a loopback address")
            } else if o[0] == 0 {
                Some("an unspecified address")
            } else if v4.is_link_local() {
                Some("a link-local address")
            } else if v4.is_multicast() {
                Some("a multicast address")
            } else if v4 == Ipv4Addr::BROADCAST {
                Some("the broadcast address")
            } else if [
                Ipv4Addr::new(100, 100, 100, 200),
                Ipv4Addr::new(168, 63, 129, 16),
                Ipv4Addr::new(192, 0, 0, 192),
            ]
            .contains(&v4)
            {
                Some("a cloud metadata address")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            if v6.is_loopback() {
                Some("a loopback address")
            } else if v6.is_unspecified() {
                Some("an unspecified address")
            } else if s[0] & 0xffc0 == 0xfe80 {
                Some("a link-local address")
            } else if v6.is_multicast() {
                Some("a multicast address")
            } else if (s[0] == 0xfd00 && s[1] == 0x0ec2)
                || [
                    Ipv6Addr::new(0xfd20, 0xce, 0, 0, 0, 0, 0, 0x254),
                    Ipv6Addr::new(0xfd00, 0xc1, 0, 0, 0, 0, 0xa9fe, 0xa9fe),
                    Ipv6Addr::new(0xfd00, 0x42, 0, 0, 0, 0, 0, 0x42),
                    Ipv6Addr::new(0xfd00, 0xa9fe, 0xa9fe, 0, 0, 0, 0, 1),
                    Ipv6Addr::new(0xfd00, 0x100, 0, 0, 0, 0, 0x100, 0x200),
                ]
                .contains(&v6)
            {
                Some("a cloud metadata address")
            } else {
                None
            }
        }
    }
}

/// What the proxy saw, for the caller's report.
#[derive(Debug, Default)]
struct Seen {
    /// `host:port` of each refused request.
    blocked: BTreeSet<String>,
    /// `host:port` of each request let through.
    contacted: BTreeSet<String>,
    /// What each request that came without the run's secret asked for.
    unauthenticated: BTreeSet<String>,
}

struct Shared {
    rules: Vec<HostRule>,
    /// The `Proxy-Authorization` value a request must carry, where the proxy
    /// is reached over a loopback port that other programs of the same user
    /// can also reach ([`EgressProxy::start`] on Windows). `None` is no
    /// check, which is right where only this run can reach the listener.
    auth: Option<String>,
    /// Most connections served at once ([`max_connections`]).
    max_connections: usize,
    seen: Mutex<Seen>,
    stop: AtomicBool,
    /// Every open client and upstream stream, shut down when the proxy stops.
    open: Mutex<std::collections::BTreeMap<u64, TcpStream>>,
    next: std::sync::atomic::AtomicU64,
}

impl Shared {
    fn new(rules: Vec<HostRule>, max_connections: usize) -> Arc<Self> {
        Self::new_with_auth(rules, max_connections, None)
    }

    fn new_with_auth(
        rules: Vec<HostRule>,
        max_connections: usize,
        auth: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            rules,
            auth,
            max_connections,
            seen: Mutex::new(Seen::default()),
            stop: AtomicBool::new(false),
            open: Mutex::new(std::collections::BTreeMap::new()),
            next: std::sync::atomic::AtomicU64::new(0),
        })
    }

    fn allowed(&self, host: &str, port: u16) -> Option<&HostRule> {
        self.rules.iter().find(|r| r.allows(host, port))
    }

    fn note(&self, blocked: bool, host: &str, port: u16) {
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        let entry = HostRule {
            host: canonical_host(host),
            port: Some(port),
        }
        .to_string();
        if blocked {
            seen.blocked.insert(entry);
        } else {
            seen.contacted.insert(entry);
        }
    }

    /// Keep a handle on `stream` so a stopping proxy can shut it down. The
    /// guard shuts it down and forgets it when its connection is done: a
    /// kept handle would otherwise hold the socket open, and the client
    /// would never see the end of a refused or finished response.
    fn track(&self, stream: &TcpStream) -> Tracked<'_> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        if let Ok(clone) = stream.try_clone() {
            self.open
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id, clone);
        }
        Tracked { shared: self, id }
    }
}

/// See [`Shared::track`].
struct Tracked<'a> {
    shared: &'a Shared,
    id: u64,
}

impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        let stream = self
            .shared
            .open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
        if let Some(s) = stream {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

/// The running proxy. Dropping it stops it: the listener closes and every
/// connection is shut down.
pub struct EgressProxy {
    shared: Arc<Shared>,
    port: u16,
    /// The secret a client must present, where the listener is reachable by
    /// other programs of the same user (Windows). Carried in the proxy URL
    /// [`Self::env`] gives the command.
    token: Option<String>,
}

impl std::fmt::Debug for EgressProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressProxy")
            .field("port", &self.port)
            .field("rules", &self.shared.rules)
            .finish()
    }
}

impl EgressProxy {
    /// Start a proxy that allows `rules`. On macOS (and Windows, for tests)
    /// it listens on the host's loopback at once, on a port the system
    /// picks. On Linux it waits for the listener a sandboxed command opens
    /// in its own network namespace ([`Self::serve`]).
    pub fn start(rules: Vec<HostRule>) -> io::Result<Self> {
        // On Windows the listener sits on a fixed loopback port that every
        // program of the sandbox user may reach, including a command of
        // another run whose network is denied. A secret per run keeps that
        // command out: without it the proxy answers 407 and forwards
        // nothing. Elsewhere the listener is reachable only from inside the
        // command's own isolation, so no secret is needed.
        let token = if cfg!(windows) {
            Some(new_token()?)
        } else {
            None
        };
        let shared = Shared::new_with_auth(
            rules,
            max_connections(),
            token.as_deref().map(basic_credentials),
        );
        #[cfg(target_os = "linux")]
        {
            Ok(Self {
                shared,
                port: NAMESPACE_PORT,
                token,
            })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let listener = bind_for_sandbox()?;
            let port = listener.local_addr()?.port();
            let proxy = Self {
                shared,
                port,
                token,
            };
            proxy.serve(listener)?;
            Ok(proxy)
        }
    }

    /// Start a proxy listening on the host's loopback on every platform:
    /// for a caller outside the sandbox, and for tests.
    pub fn start_on_host(rules: Vec<HostRule>) -> io::Result<Self> {
        Self::start_on_host_with(Shared::new(rules, max_connections()))
    }

    /// [`Self::start_on_host`] serving at most `cap` connections at once.
    #[cfg(test)]
    fn start_on_host_capped(rules: Vec<HostRule>, cap: usize) -> io::Result<Self> {
        Self::start_on_host_with(Shared::new(rules, cap))
    }

    /// [`Self::start_on_host`] that requires the secret `token`.
    #[cfg(test)]
    fn start_on_host_with_token(rules: Vec<HostRule>, token: &str) -> io::Result<Self> {
        let shared =
            Shared::new_with_auth(rules, max_connections(), Some(basic_credentials(token)));
        let mut proxy = Self::start_on_host_with(shared)?;
        proxy.token = Some(token.to_owned());
        Ok(proxy)
    }

    fn start_on_host_with(shared: Arc<Shared>) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let proxy = Self {
            shared,
            port,
            token: None,
        };
        proxy.serve(listener)?;
        Ok(proxy)
    }

    /// The network a sandboxed command that goes through this proxy gets.
    pub fn network(&self) -> Network {
        Network::Proxy { port: self.port }
    }

    /// The port clients connect to.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Accept clients on `listener` until the proxy is dropped. Each client
    /// gets a thread.
    pub fn serve(&self, listener: TcpListener) -> io::Result<()> {
        listener.set_nonblocking(true)?;
        let shared = self.shared.clone();
        std::thread::Builder::new()
            .name("kgraf-egress-accept".into())
            .spawn(move || accept_loop(listener, shared))?;
        Ok(())
    }

    /// Spawn `cmd` (built with [`Self::network`]) and, on Linux, take the
    /// listener it opened in its namespace and serve on it. The one way to
    /// start a command that uses this proxy.
    #[cfg(unix)]
    pub fn spawn(
        &self,
        cmd: &mut crate::SandboxedCommand,
    ) -> Result<std::process::Child, crate::SandboxError> {
        let mut child = cmd.spawn()?;
        if let Some(listener) = cmd.take_loopback_listener(Duration::from_secs(10)) {
            let served = listener.and_then(|l| self.serve(l).map_err(crate::SandboxError::Spawn));
            if let Err(e) = served {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        Ok(child)
    }

    /// The variables that send a command's HTTP clients through the proxy:
    /// the common proxy variables (both cases), npm's and Yarn's own, and
    /// Java's proxy properties (`JAVA_TOOL_OPTIONS`, read by every JVM,
    /// so Maven and Gradle). `NO_PROXY` is emptied, so no name bypasses it.
    ///
    /// Where the proxy holds a secret (Windows), only the URL variables carry
    /// it. `JAVA_TOOL_OPTIONS` names the proxy without it: every JVM prints
    /// that variable's whole value to its error stream on start ("Picked up
    /// JAVA_TOOL_OPTIONS: ..."), which would put the secret in the command's
    /// output and transcript, and the JDK refuses Basic proxy authentication
    /// for HTTPS tunnels by default (`jdk.http.auth.tunneling.disabledSchemes`),
    /// so the secret would not get a Maven or Gradle download through anyway.
    /// A JVM there gets the proxy's 407 with [`UNAUTHENTICATED_TEXT`], and
    /// [`Self::refused_without_secret`] lists its requests.
    pub fn env(&self) -> Vec<(String, String)> {
        let url = match &self.token {
            Some(token) => format!("http://{PROXY_USER}:{token}@127.0.0.1:{}", self.port),
            None => format!("http://127.0.0.1:{}", self.port),
        };
        let mut env: Vec<(String, String)> = [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "http_proxy",
            "https_proxy",
            "npm_config_proxy",
            "npm_config_https_proxy",
            "YARN_HTTP_PROXY",
            "YARN_HTTPS_PROXY",
        ]
        .iter()
        .map(|k| ((*k).to_owned(), url.clone()))
        .collect();
        for k in ["NO_PROXY", "no_proxy", "npm_config_noproxy"] {
            env.push((k.to_owned(), String::new()));
        }
        env.push((
            "JAVA_TOOL_OPTIONS".to_owned(),
            format!(
                "-Dhttp.proxyHost=127.0.0.1 -Dhttp.proxyPort={p} -Dhttps.proxyHost=127.0.0.1 \
                 -Dhttps.proxyPort={p} -Dhttp.nonProxyHosts=",
                p = self.port
            ),
        ));
        env
    }

    /// Variables a command must not inherit when it uses the proxy: other
    /// proxies, which would take its traffic elsewhere.
    pub const REMOVED_ENV: &'static [&'static str] =
        &["ALL_PROXY", "all_proxy", "FTP_PROXY", "ftp_proxy"];

    /// Each `host:port` a request was refused for, sorted.
    pub fn blocked(&self) -> Vec<String> {
        let seen = self.shared.seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.blocked.iter().cloned().collect()
    }

    /// Each `host:port` a request was let through to, sorted.
    pub fn contacted(&self) -> Vec<String> {
        let seen = self.shared.seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.contacted.iter().cloned().collect()
    }

    /// What each request that came without this run's secret asked for
    /// (its request target), sorted: a client that does not read the proxy
    /// variables, such as a JVM, or a program of another run. A caller
    /// reports these with a failed command, so a 407 is never the only trace.
    pub fn refused_without_secret(&self) -> Vec<String> {
        let seen = self.shared.seen.lock().unwrap_or_else(|p| p.into_inner());
        seen.unauthenticated.iter().cloned().collect()
    }
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let open = std::mem::take(&mut *self.shared.open.lock().unwrap_or_else(|p| p.into_inner()));
        for s in open.into_values() {
            let _ = s.shutdown(Shutdown::Both);
        }
    }
}

/// Most connections served at once, whatever the descriptor limit. Each
/// takes a thread or two in kGraf's own process, so a command that opens
/// connections without end must not exhaust it; a package manager opens a
/// few dozen at most.
const MAX_CONNECTIONS: usize = 256;

/// Descriptors one connection can hold in kGraf's process: the client and
/// the upstream stream, the handle on each kept to shut it down
/// ([`Shared::track`]), and the clone of each that the copy the other way
/// uses ([`relay`]).
// Windows keeps the fixed cap; the computation is still tested there.
#[cfg_attr(not(unix), allow(dead_code))]
const FDS_PER_CONNECTION: usize = 6;

/// The connection cap for a soft descriptor limit of `soft`: the
/// connections fit in a quarter of it, so a command that opens connections
/// and leaves them idle can never take the descriptors the rest of kGraf
/// needs. Measured: with a soft limit of 256 (a process started from the
/// macOS Finder or a default terminal) and a fixed cap of 256, idle
/// connections left the proxy's process unable to open a file ("Too many
/// open files"). At least 4, at most [`MAX_CONNECTIONS`].
#[cfg_attr(not(unix), allow(dead_code))]
fn max_connections_for(soft: u64) -> usize {
    let fit = soft / 4 / FDS_PER_CONNECTION as u64;
    usize::try_from(fit.clamp(4, MAX_CONNECTIONS as u64)).unwrap_or(MAX_CONNECTIONS)
}

/// [`max_connections_for`] this process's soft descriptor limit. Windows
/// has no such limit for sockets; the fixed cap holds there.
fn max_connections() -> usize {
    #[cfg(unix)]
    {
        let mut rlim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: getrlimit writes only into `rlim`.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) } == 0 {
            // `rlim_t` is 64 bits on macOS and 64-bit Linux, narrower elsewhere.
            #[allow(clippy::unnecessary_cast)]
            return max_connections_for(rlim.rlim_cur as u64);
        }
        max_connections_for(256)
    }
    #[cfg(not(unix))]
    {
        MAX_CONNECTIONS
    }
}

/// The loopback listener a sandboxed command's proxy uses. On Windows, the
/// first free port of [`crate::sandbox_user::PROXY_PORTS`]: the offline
/// sandbox user's firewall rules leave only those loopback ports open
/// (decision 0097). Elsewhere, a port the system picks.
#[cfg(not(target_os = "linux"))]
fn bind_for_sandbox() -> io::Result<TcpListener> {
    #[cfg(windows)]
    {
        let range = crate::sandbox_user::PROXY_PORTS;
        for port in range.clone() {
            match TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
                Ok(listener) => return Ok(listener),
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
                // Windows reports a port another program holds exclusively
                // as access denied.
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!(
                "every egress proxy port from {} to {} is in use",
                range.start(),
                range.end()
            ),
        ))
    }
    #[cfg(not(windows))]
    {
        TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    // Non-blocking accept with a short sleep, so the loop sees the stop flag
    // without a self-connection trick. An install opens few connections.
    let live = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while !shared.stop.load(Ordering::SeqCst) {
        // At the cap, accept nothing: a new client waits in the listen
        // backlog until a connection ends, and is then served. A refusal
        // would make a package manager back off and retry, or give up; and an
        // accepted socket already takes a descriptor.
        if live.load(Ordering::SeqCst) >= shared.max_connections {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        match listener.accept() {
            Ok((client, _)) => {
                live.fetch_add(1, Ordering::SeqCst);
                let shared = shared.clone();
                let done = live.clone();
                let spawned = std::thread::Builder::new()
                    .name("kgraf-egress-conn".into())
                    .spawn(move || {
                        let _ = handle(client, &shared);
                        done.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    live.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// A parsed request head.
struct Head {
    method: String,
    target: String,
    version: String,
    /// Header lines as they came, without the request line.
    headers: Vec<String>,
    /// Bytes read past the head (a request body, a TLS hello sent early).
    rest: Vec<u8>,
}

fn read_head(client: &mut TcpStream) -> io::Result<Head> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request head too long",
            ));
        }
        let n = client.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no request"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let text = String::from_utf8_lossy(&buf[..end]).into_owned();
    let rest = buf[end + 4..].to_vec();
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut words = first.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (words.next(), words.next(), words.next())
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad request line",
        ));
    };
    Ok(Head {
        method: method.to_owned(),
        target: target.to_owned(),
        version: version.to_owned(),
        headers: lines.map(str::to_owned).collect(),
        rest,
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn respond(client: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        client,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = client.flush();
}

/// The text a refused client reads; package managers print it.
fn blocked_text(host: &str, port: u16) -> String {
    format!(
        "kGraf blocked {host}:{port}: it is not an allowed registry host for this install. \
         Allow it in Settings (install registries) or the `install_registries` settings key, \
         then run the install again.\n"
    )
}

/// The user name in the proxy URL. The secret is the password beside it.
const PROXY_USER: &str = "kgraf";

/// What a client that came without the run's secret reads. A JVM is the
/// likely one: kGraf points it at the proxy but does not give it the secret
/// ([`EgressProxy::env`]), so its requests fail here, loudly, instead of
/// timing out against the firewall.
const UNAUTHENTICATED_TEXT: &str = "kGraf's egress proxy serves only the run it belongs to, \
    which presents the secret in its proxy variables (HTTP_PROXY, HTTPS_PROXY). Java clients \
    do not get the secret: every JVM prints JAVA_TOOL_OPTIONS on start, and the JDK refuses \
    Basic proxy authentication for HTTPS tunnels by default. Java dependency installs through \
    this proxy are not supported on Windows.\n";

/// A secret for one run, from the system random source.
fn new_token() -> io::Result<String> {
    let mut b = [0u8; 24];
    getrandom::fill(&mut b).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// `Basic ...` for `token`, the header value a client must send.
fn basic_credentials(token: &str) -> String {
    format!(
        "Basic {}",
        base64(format!("{PROXY_USER}:{token}").as_bytes())
    )
}

/// Standard base64 with padding. Written here so the proxy needs no
/// dependency for one header value.
fn base64(bytes: &[u8]) -> String {
    const SET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let b = [
            group[0],
            *group.get(1).unwrap_or(&0),
            *group.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= group.len() {
                out.push(SET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Whether `head` carries exactly the `Proxy-Authorization` value `expected`.
/// The comparison reads every byte, so it does not report how much of the
/// secret matched.
fn carries(head: &Head, expected: &str) -> bool {
    let given = head.headers.iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("proxy-authorization")
            .then(|| value.trim())
    });
    let Some(given) = given else {
        return false;
    };
    if given.len() != expected.len() {
        return false;
    }
    given
        .bytes()
        .zip(expected.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

fn handle(mut client: TcpStream, shared: &Shared) -> io::Result<()> {
    // On macOS and the other BSDs an accepted socket inherits the
    // listener's non-blocking mode (Linux's does not). Measured: without
    // this, a request that had not arrived when the proxy first read was
    // dropped as `WouldBlock`, and npm retried with back-off for minutes.
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(IDLE))?;
    client.set_write_timeout(Some(IDLE))?;
    let _client_guard = shared.track(&client);
    let head = read_head(&mut client)?;
    if let Some(expected) = &shared.auth
        && !carries(&head, expected)
    {
        shared
            .seen
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .unauthenticated
            .insert(head.target.chars().take(200).collect());
        respond(
            &mut client,
            "407 Proxy Authentication Required",
            UNAUTHENTICATED_TEXT,
        );
        return Ok(());
    }
    if head.method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_port(&head.target);
        let Some(port) = port.and_then(|p| p.parse::<u16>().ok()) else {
            respond(&mut client, "400 Bad Request", "CONNECT needs host:port\n");
            return Ok(());
        };
        let host = canonical_host(host);
        let upstream = match open_upstream(shared, &host, port) {
            Ok(u) => u,
            Err(Refusal::Blocked) => {
                respond(&mut client, "403 Forbidden", &blocked_text(&host, port));
                return Ok(());
            }
            Err(Refusal::Upstream(why)) => {
                respond(
                    &mut client,
                    "502 Bad Gateway",
                    &format!("kGraf could not reach {host}:{port}: {why}\n"),
                );
                return Ok(());
            }
        };
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?;
        let mut up = upstream;
        let _up_guard = shared.track(&up);
        if !head.rest.is_empty() {
            up.write_all(&head.rest)?;
        }
        relay(client, up);
        return Ok(());
    }
    // Absolute-form plain HTTP: `GET http://host[:port]/path HTTP/1.1`.
    let Some(after) = head
        .target
        .strip_prefix("http://")
        .or_else(|| head.target.strip_prefix("HTTP://"))
    else {
        respond(
            &mut client,
            "400 Bad Request",
            "kGraf's proxy takes CONNECT and absolute http:// requests only\n",
        );
        return Ok(());
    };
    let (authority, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, "/"),
    };
    // A user and password in the URL are never forwarded.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = split_port(authority);
    let port = match port {
        None => 80,
        Some(p) => match p.parse::<u16>() {
            Ok(n) => n,
            Err(_) => {
                respond(&mut client, "400 Bad Request", "bad port\n");
                return Ok(());
            }
        },
    };
    let host = canonical_host(host);
    let mut up = match open_upstream(shared, &host, port) {
        Ok(u) => u,
        Err(Refusal::Blocked) => {
            respond(&mut client, "403 Forbidden", &blocked_text(&host, port));
            return Ok(());
        }
        Err(Refusal::Upstream(why)) => {
            respond(
                &mut client,
                "502 Bad Gateway",
                &format!("kGraf could not reach {host}:{port}: {why}\n"),
            );
            return Ok(());
        }
    };
    let _up_guard = shared.track(&up);
    let mut out = format!("{} {} {}\r\n", head.method, path, head.version);
    for line in &head.headers {
        let name = line
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "proxy-authorization" | "proxy-connection" | "connection" | "keep-alive" | "host"
        ) {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    // The host the allowlist judged, never another name the client put in
    // its own `Host` line (a server holding several names at one address
    // would answer for that one).
    let host_line = HostRule {
        host: host.clone(),
        port: (port != 80).then_some(port),
    };
    out.push_str(&format!("Host: {host_line}\r\nConnection: close\r\n\r\n"));
    up.write_all(out.as_bytes())?;
    if !head.rest.is_empty() {
        up.write_all(&head.rest)?;
    }
    relay(client, up);
    Ok(())
}

enum Refusal {
    Blocked,
    Upstream(String),
}

/// Check `host:port` against the rules and connect. A host name is resolved
/// once, every address it resolves to that is denied ([`denied_address`]) is
/// dropped unless an address rule names it, and the connection goes to a
/// surviving address: the address checked is the address dialed.
fn open_upstream(shared: &Shared, host: &str, port: u16) -> Result<TcpStream, Refusal> {
    let Some(rule) = shared.allowed(host, port) else {
        shared.note(true, host, port);
        return Err(Refusal::Blocked);
    };
    let literal = host.parse::<IpAddr>().ok();
    let addrs: Vec<SocketAddr> = match literal {
        Some(ip) => vec![SocketAddr::new(ip, port)],
        None => (host, port)
            .to_socket_addrs()
            .map_err(|e| Refusal::Upstream(format!("cannot resolve the name: {e}")))?
            .collect(),
    };
    let explicit = |ip: IpAddr| {
        shared
            .rules
            .iter()
            .any(|r| r.is_address() && r.allows(&ip.to_string(), port))
    };
    let mut refused = None;
    let usable: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| {
            if rule.is_address() || explicit(a.ip()) {
                return true;
            }
            match denied_address(a.ip()) {
                Some(why) => {
                    refused = Some(why);
                    false
                }
                None => true,
            }
        })
        .collect();
    if usable.is_empty() {
        shared.note(true, host, port);
        return Err(Refusal::Upstream(match refused {
            Some(why) => format!("the name resolves to {why}, which is not allowed"),
            None => "the name resolves to no address".to_owned(),
        }));
    }
    let mut last = None;
    for addr in usable {
        match TcpStream::connect_timeout(&addr, CONNECT_WAIT) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(IDLE));
                let _ = s.set_write_timeout(Some(IDLE));
                shared.note(false, host, port);
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(Refusal::Upstream(
        last.map(|e| e.to_string())
            .unwrap_or_else(|| "no address answered".to_owned()),
    ))
}

/// Copy both ways until either side closes.
fn relay(client: TcpStream, upstream: TcpStream) {
    let (Ok(mut c2), Ok(mut u2)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    let mut c1 = client;
    let mut u1 = upstream;
    let up = std::thread::Builder::new()
        .name("kgraf-egress-up".into())
        .spawn(move || {
            let _ = io::copy(&mut c1, &mut u1);
            let _ = u1.shutdown(Shutdown::Write);
        });
    let _ = io::copy(&mut u2, &mut c2);
    let _ = c2.shutdown(Shutdown::Write);
    if let Ok(t) = up {
        let _ = t.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(t: &str) -> HostRule {
        HostRule::parse(t).expect(t)
    }

    #[test]
    fn entries_parse_and_match_by_name_wildcard_and_port() {
        let npm = rule("registry.npmjs.org");
        assert!(npm.allows("registry.npmjs.org", 443));
        assert!(
            npm.allows("REGISTRY.NPMJS.ORG.", 80),
            "case and a trailing dot"
        );
        assert!(!npm.allows("registry.npmjs.org", 8443), "an unnamed port");
        assert!(!npm.allows("evil-registry.npmjs.org", 443));
        assert!(!npm.allows("registry.npmjs.org.evil.com", 443));
        let wild = rule("*.example.com");
        assert!(wild.allows("a.b.example.com", 443));
        assert!(!wild.allows("example.com", 443), "not the domain itself");
        assert!(!wild.allows("badexample.com", 443));
        let local = rule("127.0.0.1:4873");
        assert!(local.allows("127.0.0.1", 4873));
        assert!(!local.allows("127.0.0.1", 4874));
        assert!(!local.allows("127.0.0.1", 443));
        let v6 = rule("[::1]:8080");
        assert!(v6.allows("[::1]", 8080));
        assert_eq!(v6.to_string(), "[::1]:8080");
        for bad in [
            "",
            "*",
            "https://registry.npmjs.org",
            "registry.npmjs.org/path",
            "a.*.com",
            "*.10.0.0.1",
            "host:0",
            "host:99999",
            "ho st",
        ] {
            assert!(HostRule::parse(bad).is_err(), "{bad:?} must be refused");
        }
    }

    /// Names built to look like an allowed host never match it: a look-alike
    /// letter, a suffix or prefix, an address spelled another way, a port
    /// hidden in the name. The proxy dials the name it judged, so a name
    /// that does not match is never dialed.
    #[test]
    fn look_alike_names_never_match_an_allowed_host() {
        let npm = rule("registry.npmjs.org");
        for name in [
            "registry.npmjs.org.evil.example",
            "evilregistry.npmjs.org",
            "registry.npmjs.org@evil.example",
            "registry.npmjs.org%2eevil.example",
            "registry.npmjs.or\u{0261}",
            "xn--registry-npmjs-org",
            "registry.npmjs.org:443",
        ] {
            assert!(!npm.allows(name, 443), "{name:?} matched");
        }
        let (host, port) = split_port("registry.npmjs.org:443@evil.example");
        assert_eq!(host, "registry.npmjs.org");
        assert!(port.and_then(|p| p.parse::<u16>().ok()).is_none());
        let local = rule("127.0.0.1:4873");
        for name in ["2130706433", "127.1", "0x7f.0.0.1", "::ffff:127.0.0.1"] {
            assert!(!local.allows(name, 4873), "{name:?} matched");
        }
    }

    #[test]
    fn denied_address_classes() {
        for (ip, class) in [
            ("127.0.0.1", "a loopback address"),
            ("::1", "a loopback address"),
            ("::ffff:127.0.0.1", "a loopback address"),
            ("0.0.0.0", "an unspecified address"),
            ("169.254.169.254", "a link-local address"),
            ("fe80::1", "a link-local address"),
            ("224.0.0.1", "a multicast address"),
            ("255.255.255.255", "the broadcast address"),
            ("100.100.100.200", "a cloud metadata address"),
            ("fd00:ec2::254", "a cloud metadata address"),
        ] {
            assert_eq!(denied_address(ip.parse().unwrap()), Some(class), "{ip}");
        }
        for ip in ["104.16.0.1", "10.0.0.5", "192.168.1.1", "2606:4700::1"] {
            assert_eq!(denied_address(ip.parse().unwrap()), None, "{ip}");
        }
    }

    /// A tiny origin server: answers each connection with `body` and keeps
    /// the request it read.
    fn origin(body: &'static str) -> (u16, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s2 = seen.clone();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(mut c) = c else { continue };
                let mut buf = [0u8; 4096];
                let n = c.read(&mut buf).unwrap_or(0);
                s2.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let _ = write!(
                    c,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (port, seen)
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        for (plain, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("kgraf:abc", "a2dyYWY6YWJj"),
        ] {
            assert_eq!(base64(plain.as_bytes()), want, "{plain}");
        }
        // Every byte value, so no group is encoded with the wrong bits.
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64(&all).len(), 344);
        assert_eq!(&base64(&all)[..8], "AAECAwQF");
    }

    #[test]
    fn a_proxy_with_a_secret_serves_only_requests_that_carry_it() {
        let (origin_port, hits) = origin("from-the-registry");
        let token = "0f1e2d3c4b5a69788796a5b4c3d2e1f0";
        let proxy = EgressProxy::start_on_host_with_token(
            vec![rule(&format!("127.0.0.1:{origin_port}"))],
            token,
        )
        .expect("proxy");
        let request = |extra: &str| {
            format!("GET http://127.0.0.1:{origin_port}/pkg HTTP/1.1\r\nHost: x\r\n{extra}\r\n")
        };
        let refused = [
            String::new(),
            "Proxy-Authorization: Basic \r\n".to_owned(),
            format!("Proxy-Authorization: {}\r\n", basic_credentials("wrong")),
            // The right length, one byte different.
            format!(
                "Proxy-Authorization: {}\r\n",
                basic_credentials(&format!("{}1", &token[..token.len() - 1]))
            ),
        ];
        for extra in refused {
            let out = through(proxy.port(), &request(&extra));
            assert!(out.starts_with("HTTP/1.1 407 "), "{extra:?} gave {out}");
        }
        assert!(hits.lock().unwrap().is_empty(), "nothing was forwarded");
        let out = through(
            proxy.port(),
            &request(&format!(
                "Proxy-Authorization: {}\r\n",
                basic_credentials(token)
            )),
        );
        assert!(out.ends_with("from-the-registry"), "{out}");
        assert_eq!(hits.lock().unwrap().len(), 1);
        // The secret never reaches the host the command asked for.
        assert!(!hits.lock().unwrap()[0].contains(token));
    }

    #[test]
    fn a_proxy_without_a_secret_takes_any_request() {
        let (origin_port, _) = origin("hello");
        let proxy =
            EgressProxy::start_on_host(vec![rule(&format!("127.0.0.1:{origin_port}"))]).unwrap();
        let out = through(
            proxy.port(),
            &format!("GET http://127.0.0.1:{origin_port}/p HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(out.ends_with("hello"), "{out}");
        assert!(!proxy.env().iter().any(|(_, v)| v.contains('@')));
    }

    #[test]
    fn the_commands_proxy_variables_carry_the_secret() {
        let (origin_port, _) = origin("hello");
        let token = "abc123";
        let proxy = EgressProxy::start_on_host_with_token(
            vec![rule(&format!("127.0.0.1:{origin_port}"))],
            token,
        )
        .unwrap();
        let env = proxy.env();
        let url = format!("http://kgraf:{token}@127.0.0.1:{}", proxy.port());
        for key in [
            "HTTP_PROXY",
            "https_proxy",
            "npm_config_proxy",
            "YARN_HTTP_PROXY",
        ] {
            let got = env.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
            assert_eq!(got, Some(url.as_str()), "{key}");
        }
        // Java is pointed at the proxy without the secret: a JVM prints
        // this variable on every start.
        let java = env
            .iter()
            .find(|(k, _)| k == "JAVA_TOOL_OPTIONS")
            .map(|(_, v)| v.clone())
            .expect("java options");
        assert!(!java.contains(token), "{java}");
        assert!(!java.contains("proxyPassword"), "{java}");
        assert!(
            java.contains(&format!("-Dhttps.proxyPort={}", proxy.port())),
            "{java}"
        );
        assert!(java.ends_with("-Dhttp.nonProxyHosts="), "{java}");
        // No other variable but the URL ones carries it.
        for (key, value) in &env {
            if value.contains(token) {
                assert!(value == &url, "{key} carries the secret: {value}");
            }
        }
    }

    #[test]
    fn a_request_without_the_secret_is_refused_loudly_and_listed() {
        let (origin_port, hits) = origin("from-the-registry");
        let proxy = EgressProxy::start_on_host_with_token(
            vec![rule(&format!("127.0.0.1:{origin_port}"))],
            "0f1e2d3c4b5a6978",
        )
        .unwrap();
        // What a JVM sends with only the proxy host and port: a tunnel
        // request with no credentials.
        let out = through(
            proxy.port(),
            &format!("CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(out.starts_with("HTTP/1.1 407 "), "{out}");
        assert!(out.contains("Java dependency installs"), "{out}");
        assert!(hits.lock().unwrap().is_empty());
        assert_eq!(
            proxy.refused_without_secret(),
            vec![format!("127.0.0.1:{origin_port}")]
        );
        assert!(proxy.blocked().is_empty(), "not an allowlist refusal");
    }

    fn through(proxy: u16, request: &str) -> String {
        let mut c = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        c.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        out
    }

    #[test]
    fn plain_http_to_an_allowed_host_is_forwarded_in_origin_form() {
        let (port, seen) = origin("hello");
        let proxy = EgressProxy::start_on_host(vec![rule(&format!("127.0.0.1:{port}"))]).unwrap();
        let out = through(
            proxy.port(),
            &format!(
                "GET http://user:pw@127.0.0.1:{port}/pkg HTTP/1.1\r\nHost: other.example\r\n\
                 Proxy-Authorization: Basic eDp5\r\nConnection: keep-alive\r\n\r\n"
            ),
        );
        assert!(out.ends_with("hello"), "{out}");
        let req = seen.lock().unwrap()[0].clone();
        assert!(req.starts_with("GET /pkg HTTP/1.1\r\n"), "{req}");
        assert!(!req.contains("Proxy-Authorization"), "{req}");
        assert!(!req.contains("user:pw"), "{req}");
        assert!(req.contains("Connection: close"), "{req}");
        assert!(
            req.contains(&format!("Host: 127.0.0.1:{port}\r\n")),
            "the judged host, not the client's: {req}"
        );
        assert!(!req.contains("other.example"), "{req}");
        assert_eq!(proxy.contacted(), vec![format!("127.0.0.1:{port}")]);
        assert!(proxy.blocked().is_empty());
    }

    #[test]
    fn a_host_off_the_list_is_refused_named_and_never_contacted() {
        let (allowed, _) = origin("ok");
        let (other, other_seen) = origin("secret");
        let proxy =
            EgressProxy::start_on_host(vec![rule(&format!("127.0.0.1:{allowed}"))]).unwrap();
        let out = through(
            proxy.port(),
            &format!("GET http://127.0.0.1:{other}/x HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        assert!(
            out.contains(&format!("kGraf blocked 127.0.0.1:{other}")),
            "{out}"
        );
        let out = through(
            proxy.port(),
            &format!("CONNECT 127.0.0.1:{other} HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        let out = through(
            proxy.port(),
            "CONNECT example.invalid:443 HTTP/1.1\r\nHost: example.invalid\r\n\r\n",
        );
        assert!(out.starts_with("HTTP/1.1 403"), "{out}");
        std::thread::sleep(Duration::from_millis(100));
        assert!(other_seen.lock().unwrap().is_empty(), "never contacted");
        assert_eq!(
            proxy.blocked(),
            vec![
                format!("127.0.0.1:{other}"),
                "example.invalid:443".to_owned()
            ]
        );
    }

    #[test]
    fn connect_tunnels_bytes_both_ways_to_an_allowed_host() {
        let (port, seen) = origin("tunneled");
        let proxy = EgressProxy::start_on_host(vec![rule(&format!("127.0.0.1:{port}"))]).unwrap();
        let mut c = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        write!(c, "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut head = [0u8; 39];
        c.read_exact(&mut head).unwrap();
        assert_eq!(&head[..], b"HTTP/1.1 200 Connection Established\r\n\r\n");
        c.write_all(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        assert!(out.ends_with("tunneled"), "{out}");
        assert!(seen.lock().unwrap()[0].starts_with("GET / HTTP/1.1"));
    }

    /// A client that sends its request a moment after it connects is still
    /// served. On macOS an accepted socket inherits the listener's
    /// non-blocking mode, and the proxy once dropped such a request.
    #[test]
    fn a_request_that_arrives_late_is_still_read() {
        let (port, _) = origin("late");
        let proxy = EgressProxy::start_on_host(vec![rule(&format!("127.0.0.1:{port}"))]).unwrap();
        for _ in 0..3 {
            let mut c = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
            c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            std::thread::sleep(Duration::from_millis(150));
            write!(
                c,
                "GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: x\r\n\r\n"
            )
            .unwrap();
            let mut out = String::new();
            let _ = c.read_to_string(&mut out);
            assert!(out.ends_with("late"), "{out:?}");
        }
    }

    #[test]
    fn an_allowed_name_that_resolves_to_loopback_is_refused() {
        let (port, seen) = origin("x");
        // `localhost` resolves to loopback; a name rule does not admit it.
        let proxy = EgressProxy::start_on_host(vec![rule(&format!("localhost:{port}"))]).unwrap();
        let out = through(
            proxy.port(),
            &format!("GET http://localhost:{port}/ HTTP/1.1\r\nHost: x\r\n\r\n"),
        );
        assert!(out.starts_with("HTTP/1.1 502"), "{out}");
        assert!(out.contains("loopback"), "{out}");
        assert!(seen.lock().unwrap().is_empty());
    }

    /// The proxy runs in kGraf's own process, and every connection the
    /// sandboxed command opens costs kGraf up to six descriptors (the client,
    /// the upstream, a handle on each to shut it down, and a clone of each for
    /// the copy the other way). A process started from the macOS Finder or a
    /// default terminal has a soft limit of 256: 256 idle connections from an
    /// install script left kGraf unable to open a file. The cap follows the
    /// limit, using at most a quarter of it.
    #[test]
    fn the_connection_cap_keeps_to_a_quarter_of_the_descriptor_limit() {
        for soft in [64u64, 256, 1024, 10_240] {
            let cap = max_connections_for(soft) as u64;
            assert!(cap >= 4, "{soft}: some connections are served");
            assert!(
                cap * FDS_PER_CONNECTION as u64 <= soft / 4 || cap == 4,
                "{soft}: {cap} connections can take {} descriptors",
                cap * FDS_PER_CONNECTION as u64
            );
        }
        assert_eq!(max_connections_for(u64::MAX), MAX_CONNECTIONS);
    }

    /// A client past the cap waits for a free place and is then served; it
    /// is not refused (a refused connection makes a package manager back off
    /// and retry for minutes, or give up).
    #[test]
    fn a_client_past_the_cap_waits_and_is_then_served() {
        let (port, _) = origin("served");
        let proxy =
            EgressProxy::start_on_host_capped(vec![rule(&format!("127.0.0.1:{port}"))], 1).unwrap();
        // One idle client takes the only place.
        let idle = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let p = proxy.port();
        let waiting = std::thread::spawn(move || {
            through(
                p,
                &format!("GET http://127.0.0.1:{port}/ HTTP/1.1\r\nHost: x\r\n\r\n"),
            )
        });
        std::thread::sleep(Duration::from_millis(400));
        assert!(!waiting.is_finished(), "served past the cap");
        drop(idle);
        let out = waiting.join().unwrap();
        assert!(out.ends_with("served"), "{out}");
    }

    #[test]
    fn the_env_points_every_client_at_the_proxy_and_empties_no_proxy() {
        let proxy = EgressProxy::start_on_host(Vec::new()).unwrap();
        let env: std::collections::BTreeMap<_, _> = proxy.env().into_iter().collect();
        let url = format!("http://127.0.0.1:{}", proxy.port());
        for k in [
            "HTTPS_PROXY",
            "https_proxy",
            "HTTP_PROXY",
            "npm_config_https_proxy",
        ] {
            assert_eq!(env[k], url, "{k}");
        }
        assert_eq!(env["NO_PROXY"], "");
        assert!(env["JAVA_TOOL_OPTIONS"].contains(&format!("-Dhttps.proxyPort={}", proxy.port())));
    }
}
