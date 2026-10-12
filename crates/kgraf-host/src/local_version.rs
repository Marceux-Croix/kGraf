//! The Ollama server's version, asked of the server itself at run start.
//!
//! A server too old for a model fails later with an unclear error. Ollama
//! answers `GET /api/version` with `{"version":"0.34.4"}`, and `POST /api/show`
//! with a `requires` field (the oldest server that can run the model) for a
//! model that has one. Only the route's own server is asked: no registry and
//! no release page is contacted.
//!
//! Outcomes of [`check_ollama_versions`]:
//! - a model's `requires` is newer than the server: [`NotReady::ServerTooOld`],
//!   because every call to that model would fail;
//! - the server is older than [`OLLAMA_VERIFIED_VERSION`]: a notice;
//! - the version cannot be learned (another server behind the route, an old
//!   Ollama without the endpoint, a development build): a notice, never a
//!   failure;
//! - otherwise the notice only records the version.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use semver::Version;

use crate::config::RouteSpec;
use crate::error::NotReady;
use crate::host::Notice;

/// The route this check covers. Other local routes speak other protocols.
const ROUTE: &str = "ollama";

/// The Ollama version the `ollama` route was last verified against: the
/// server reported by `GET /api/version` during the 2026-09-28 live exchange.
/// An older server gets a notice, not a failure.
pub(crate) const OLLAMA_VERIFIED_VERSION: &str = "0.34.4";

/// Longest reply read from the server. `/api/show` without `verbose` is a few
/// kilobytes; the bound stops a server that never ends its reply.
const MAX_REPLY: u64 = 8 * 1024 * 1024;

/// An `http://` base URL split for the native API: host, port, and the path
/// before `/v1` (empty for a server at the root).
#[derive(Debug, PartialEq, Eq)]
struct Native {
    host: String,
    port: u16,
    prefix: String,
}

/// The native API root of an OpenAI-compatible base URL: the same address
/// without the `/v1` suffix. A moved base URL keeps its host, port and any
/// path in front of `/v1` (a reverse proxy). `None` for a URL that is not
/// plain `http://` (the check does not speak TLS).
fn native_of(base_url: &str) -> Option<Native> {
    let rest = base_url.strip_prefix("http://")?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, String::new()),
    };
    let path = path.split(['?', '#']).next().unwrap_or("");
    let path = path.trim_end_matches('/');
    let prefix = path.strip_suffix("/v1").unwrap_or(path);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.contains(']') => (h, p.parse().ok()?),
        _ => (authority, 80),
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    if host.is_empty() {
        return None;
    }
    Some(Native {
        host: host.to_owned(),
        port,
        prefix: prefix.to_owned(),
    })
}

/// One HTTP/1.0 exchange: the status and the body. HTTP/1.0 keeps a server
/// from chunking its reply, so the body is read to the end of the stream.
/// `timeout` bounds the whole exchange, connect included.
fn exchange(
    native: &Native,
    method: &str,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), String> {
    use std::net::ToSocketAddrs;
    let deadline = Instant::now() + timeout;
    let left = || {
        deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| format!("no answer within {} s", timeout.as_secs()))
    };
    let addrs = (native.host.as_str(), native.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve `{}`: {e}", native.host))?;
    let mut last = format!("`{}` resolves to no address", native.host);
    let mut stream = None;
    for addr in addrs {
        match std::net::TcpStream::connect_timeout(&addr, left()?) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = format!("{addr}: {e}"),
        }
    }
    let mut stream = stream.ok_or(last)?;
    let host = if native.host.contains(':') {
        format!("[{}]:{}", native.host, native.port)
    } else {
        format!("{}:{}", native.host, native.port)
    };
    let payload = body.unwrap_or("");
    let head = format!(
        "{method} {}{path} HTTP/1.0\r\nHost: {host}\r\nAccept: application/json\r\n\
         Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        native.prefix,
        payload.len()
    );
    stream
        .set_write_timeout(Some(left()?))
        // One write: a server that answers on the head and closes must not
        // reset the connection over a body still in flight.
        .and_then(|()| stream.write_all(format!("{head}{payload}").as_bytes()))
        .map_err(|e| format!("cannot send the request: {e}"))?;
    let mut reply = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        stream
            .set_read_timeout(Some(left()?))
            .map_err(|e| e.to_string())?;
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                reply.extend_from_slice(&buf[..n]);
                if reply.len() as u64 > MAX_REPLY {
                    return Err("the reply is larger than expected".to_owned());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(format!("no answer within {} s", timeout.as_secs()));
            }
            Err(e) => return Err(format!("cannot read the reply: {e}")),
        }
    }
    let text = String::from_utf8_lossy(&reply);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "the reply is not HTTP".to_owned())?;
    let status = head
        .lines()
        .next()
        .filter(|l| l.starts_with("HTTP/"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| "the reply is not HTTP".to_owned())?;
    Ok((status, body.to_owned()))
}

/// A release version from the text a server reported. A leading `v` is
/// dropped; semver orders `0.30.0-rc1` before `0.30.0`.
fn parse_version(text: &str) -> Result<Version, String> {
    let text = text.trim();
    Version::parse(text.strip_prefix('v').unwrap_or(text))
        .map_err(|e| format!("`{text}` is not a version: {e}"))
}

/// The server's version from `GET /api/version`, or why it is unknown.
/// A source build reports `0.0.0`, which says nothing about its features, so
/// it counts as unknown.
fn server_version(native: &Native, timeout: Duration) -> Result<Version, String> {
    let (status, body) = exchange(native, "GET", "/api/version", None, timeout)?;
    if status != 200 {
        return Err(format!("/api/version answered status {status}"));
    }
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| "/api/version did not answer with JSON".to_owned())?;
    let text = json
        .get("version")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "/api/version has no `version` field".to_owned())?;
    let version = parse_version(text)?;
    if version == Version::new(0, 0, 0) {
        return Err("the server reports version 0.0.0 (a development build)".to_owned());
    }
    Ok(version)
}

/// The oldest server that runs `model` (`requires` of `POST /api/show`).
/// `Ok(None)`: Ollama answered and states no minimum, or does not know the
/// model (the first call to it says so). `Err`: the answer is not Ollama's.
fn model_minimum(
    native: &Native,
    model: &str,
    timeout: Duration,
) -> Result<Option<Version>, String> {
    let request = serde_json::json!({ "model": model }).to_string();
    let (status, body) = exchange(native, "POST", "/api/show", Some(&request), timeout)?;
    let json: serde_json::Value = serde_json::from_str(&body)
        .map_err(|_| format!("/api/show answered status {status} without JSON"))?;
    if status == 404 && json.get("error").is_some() {
        return Ok(None);
    }
    if status != 200 {
        return Err(format!("/api/show answered status {status}"));
    }
    match json.get("requires").and_then(|r| r.as_str()) {
        Some(text) => parse_version(text).map(Some),
        None => Ok(None),
    }
}

/// For the `ollama` route's server: learn its version, record it as a notice,
/// and refuse a run whose model needs a newer server.
///
/// `routes` are the planner and worker routes (the models a run calls). A
/// route that is not `ollama`, or is in-process, is skipped. Called after the
/// reachability check, so a refused connection is already reported there.
/// `timeout` bounds each request; nothing is retried.
pub(crate) fn check_ollama_versions(
    routes: &[RouteSpec],
    in_process: &dyn Fn(&str) -> bool,
    moved: &dyn Fn(&str) -> Option<String>,
    timeout: Duration,
    verified: &Version,
    notices: &mut dyn FnMut(Notice),
) -> Result<(), NotReady> {
    if in_process(ROUTE) {
        return Ok(());
    }
    let mut models: Vec<&str> = Vec::new();
    for r in routes.iter().filter(|r| r.route == ROUTE) {
        if !models.contains(&r.model.as_str()) {
            models.push(&r.model);
        }
    }
    if models.is_empty() {
        return Ok(());
    }
    let Ok(entry) = kgraf_fleet::entry(ROUTE) else {
        return Ok(());
    };
    let endpoint = match moved(ROUTE) {
        Some(url) => url,
        None => match kgraf_fleet::local_endpoint(entry) {
            Ok(url) => url,
            // `check_reachable` reported an unusable address already.
            Err(_) => return Ok(()),
        },
    };
    let unknown = |why: &str| {
        Notice::LocalServer(format!(
            "route `{ROUTE}`: the Ollama version at {endpoint} is unknown ({why}), so no model's \
             minimum version was checked"
        ))
    };
    let Some(native) = native_of(&endpoint) else {
        notices(unknown("the check speaks plain http only"));
        return Ok(());
    };
    let server = match server_version(&native, timeout) {
        Ok(v) => v,
        Err(why) => {
            notices(unknown(&why));
            return Ok(());
        }
    };
    notices(Notice::LocalServer(if server < *verified {
        format!(
            "route `{ROUTE}`: Ollama {server} at {endpoint} is older than {verified}, the version \
             this route was last verified with; update Ollama if a call fails"
        )
    } else {
        format!("route `{ROUTE}`: Ollama {server} at {endpoint}")
    }));
    for model in models {
        match model_minimum(&native, model, timeout) {
            Ok(Some(minimum)) if minimum > server => {
                return Err(NotReady::ServerTooOld {
                    route: ROUTE.to_owned(),
                    model: model.to_owned(),
                    endpoint,
                    server: server.to_string(),
                    minimum: minimum.to_string(),
                });
            }
            Ok(_) => {}
            Err(why) => notices(Notice::LocalServer(format!(
                "route `{ROUTE}`: the minimum Ollama version of model `{model}` is unknown \
                 ({why})"
            ))),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// What a fake server answers to a path and request body: status, body.
    type Handler = Box<dyn Fn(&str, &str) -> (u16, String) + Send + Sync>;

    /// A server on a loopback port. The thread ends with the test process.
    fn serve(handler: Handler) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                // Read until the head and the declared body are in.
                loop {
                    let n = stream.read(&mut buf).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).into_owned();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let want = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if body.len() >= want {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&raw).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("").to_owned();
                let body = text.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                let (status, reply) = handler(&path, body);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            }
        });
        format!("127.0.0.1:{}", addr.port())
    }

    /// An Ollama at `version`; `/api/show` gives `requires` when set.
    fn ollama(version: &'static str, requires: Option<&'static str>) -> String {
        serve(Box::new(move |path, _| match path {
            "/api/version" => (200, format!(r#"{{"version":"{version}"}}"#)),
            "/api/show" => match requires {
                Some(r) => (200, format!(r#"{{"requires":"{r}","details":{{}}}}"#)),
                None => (200, r#"{"details":{}}"#.to_owned()),
            },
            _ => (404, "{}".to_owned()),
        }))
    }

    fn spec(route: &str, model: &str) -> RouteSpec {
        RouteSpec {
            route: route.to_owned(),
            model: model.to_owned(),
        }
    }

    /// Run the check against `base_url` with the usual baseline.
    fn run(base_url: &str, models: &[&str]) -> (Result<(), NotReady>, Vec<String>) {
        let routes: Vec<RouteSpec> = models.iter().map(|m| spec("ollama", m)).collect();
        let mut seen = Vec::new();
        let url = base_url.to_owned();
        let result = check_ollama_versions(
            &routes,
            &|_| false,
            &|r| (r == "ollama").then(|| url.clone()),
            Duration::from_secs(3),
            &Version::parse(OLLAMA_VERIFIED_VERSION).expect("baseline"),
            &mut |n| match n {
                Notice::LocalServer(s) => seen.push(s),
                other => panic!("{other:?}"),
            },
        );
        (result, seen)
    }

    #[test]
    fn the_native_root_drops_v1_and_keeps_a_moved_address() {
        let native = |u: &str| native_of(u).expect(u);
        assert_eq!(native("http://localhost:11434/v1").prefix, "");
        assert_eq!(native("http://localhost:11434/v1/").prefix, "");
        assert_eq!(native("http://localhost:11434").port, 11434);
        assert_eq!(native("http://10.0.0.5:9999/v1").host, "10.0.0.5");
        assert_eq!(native("http://10.0.0.5:9999/v1").port, 9999);
        assert_eq!(native("http://gw/ollama/v1").prefix, "/ollama");
        assert_eq!(native("http://gw/ollama/v1").port, 80);
        assert_eq!(native("http://[::1]:11434/v1").host, "::1");
        assert!(native_of("https://localhost:11434/v1").is_none());
        assert!(native_of("http:///v1").is_none());
    }

    #[test]
    fn versions_compare_semantically_with_pre_releases() {
        let v = |t: &str| parse_version(t).expect(t);
        assert!(v("0.34.4") > v("0.30.0"));
        assert!(v("0.9.0") < v("0.30.0"));
        assert!(v("0.30.0-rc1") < v("0.30.0"));
        assert!(v("0.30.0-rc2") > v("0.30.0-rc1"));
        assert_eq!(v("v0.30.0"), v("0.30.0"));
        assert!(parse_version("latest").is_err());
    }

    /// The main case: a model whose `requires` is newer than the server fails
    /// the readiness check, naming the model, both versions and the fix.
    #[test]
    fn a_model_that_requires_a_newer_server_fails_naming_both_versions() {
        let host = ollama("0.29.1", Some("0.30.0"));
        let (result, seen) = run(&format!("http://{host}/v1"), &["qwen3.6:35b"]);
        let err = result.expect_err("the server is too old");
        let text = err.to_string();
        assert!(text.contains("qwen3.6:35b"), "{text}");
        assert!(text.contains("0.29.1") && text.contains("0.30.0"), "{text}");
        assert!(text.contains("update Ollama"), "{text}");
        assert!(matches!(err, NotReady::ServerTooOld { .. }), "{err:?}");
        // The version was recorded before the refusal.
        assert!(seen.iter().any(|s| s.contains("Ollama 0.29.1")), "{seen:?}");
    }

    #[test]
    fn a_pre_release_server_is_older_than_its_release() {
        let host = ollama("0.30.0-rc1", Some("0.30.0"));
        let (result, _) = run(&format!("http://{host}/v1"), &["m"]);
        assert!(result.is_err());
    }

    #[test]
    fn a_server_older_than_the_verified_version_gets_a_notice_and_the_run_goes_on() {
        let host = ollama("0.31.0", Some("0.30.0"));
        let (result, seen) = run(&format!("http://{host}/v1"), &["qwen3.6:35b"]);
        assert_eq!(result, Ok(()));
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("Ollama 0.31.0"), "{seen:?}");
        assert!(seen[0].contains("older than 0.34.4"), "{seen:?}");
    }

    #[test]
    fn a_current_server_only_records_its_version() {
        let host = ollama("0.34.4", Some("0.30.0"));
        let (result, seen) = run(&format!("http://{host}/v1"), &["qwen3.6:35b"]);
        assert_eq!(result, Ok(()));
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("Ollama 0.34.4"), "{seen:?}");
        assert!(!seen[0].contains("older"), "{seen:?}");
        // A newer server and a model with no `requires` are as quiet.
        let host = ollama("0.40.0", None);
        let (result, seen) = run(&format!("http://{host}/v1"), &["llama3"]);
        assert_eq!(result, Ok(()));
        assert_eq!(seen.len(), 1, "{seen:?}");
    }

    #[test]
    fn missing_endpoints_give_an_unknown_notice_and_the_run_goes_on() {
        // No `/api/version`: another server, or an Ollama without it.
        let host = serve(Box::new(|_, _| (404, "not found".to_owned())));
        let (result, seen) = run(&format!("http://{host}/v1"), &["m"]);
        assert_eq!(result, Ok(()));
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert!(seen[0].contains("is unknown"), "{seen:?}");
        // A body that is not JSON.
        let host = serve(Box::new(|_, _| (200, "<html>hi</html>".to_owned())));
        let (result, seen) = run(&format!("http://{host}/v1"), &["m"]);
        assert_eq!(result, Ok(()));
        assert!(seen[0].contains("is unknown"), "{seen:?}");
        // A source build says 0.0.0.
        let host = ollama("0.0.0", Some("0.30.0"));
        let (result, seen) = run(&format!("http://{host}/v1"), &["m"]);
        assert_eq!(result, Ok(()));
        assert!(seen[0].contains("is unknown"), "{seen:?}");
        // A TLS address is not checked.
        let (result, seen) = run("https://localhost:11434/v1", &["m"]);
        assert_eq!(result, Ok(()));
        assert!(seen[0].contains("is unknown"), "{seen:?}");
    }

    #[test]
    fn a_failed_show_names_the_model_and_a_model_not_pulled_is_silent() {
        // `/api/show` is not Ollama's answer.
        let host = serve(Box::new(|path, _| match path {
            "/api/version" => (200, r#"{"version":"0.34.4"}"#.to_owned()),
            _ => (500, "boom".to_owned()),
        }));
        let (result, seen) = run(&format!("http://{host}/v1"), &["m"]);
        assert_eq!(result, Ok(()));
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(
            seen[1].contains("model `m`") && seen[1].contains("unknown"),
            "{seen:?}"
        );
        // Ollama's own "model not found" is not an unknown version.
        let host = serve(Box::new(|path, _| match path {
            "/api/version" => (200, r#"{"version":"0.34.4"}"#.to_owned()),
            _ => (404, r#"{"error":"model 'm' not found"}"#.to_owned()),
        }));
        let (result, seen) = run(&format!("http://{host}/v1"), &["m"]);
        assert_eq!(result, Ok(()));
        assert_eq!(seen.len(), 1, "{seen:?}");
    }

    /// The request goes to the moved address, under its path prefix, and asks
    /// about the model the run uses.
    #[test]
    fn a_moved_base_url_is_the_one_asked() {
        let paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = paths.clone();
        let host = serve(Box::new(move |path, body| {
            log.lock().expect("log").push(format!("{path} {body}"));
            match path.trim_start_matches("/gw") {
                "/api/version" => (200, r#"{"version":"0.34.4"}"#.to_owned()),
                _ => (200, r#"{"requires":"0.30.0"}"#.to_owned()),
            }
        }));
        let (result, seen) = run(&format!("http://{host}/gw/v1"), &["qwen3.6:35b"]);
        assert_eq!(result, Ok(()));
        assert!(
            seen[0].contains(&format!("http://{host}/gw/v1")),
            "{seen:?}"
        );
        let paths = paths.lock().expect("log").clone();
        assert_eq!(paths[0], "/gw/api/version ", "{paths:?}");
        assert_eq!(
            paths[1], r#"/gw/api/show {"model":"qwen3.6:35b"}"#,
            "{paths:?}"
        );
    }

    #[test]
    fn only_the_ollama_route_is_asked() {
        let routes = [spec("llama-cpp", "m"), spec("kimi", "k")];
        let result = check_ollama_versions(
            &routes,
            &|_| false,
            &|_| Some("http://127.0.0.1:0/v1".to_owned()),
            Duration::from_secs(1),
            &Version::new(0, 34, 4),
            &mut |n| panic!("{n:?}"),
        );
        assert_eq!(result, Ok(()));
        // An in-process `ollama` has no server.
        let routes = [spec("ollama", "m")];
        let result = check_ollama_versions(
            &routes,
            &|r| r == "ollama",
            &|_| Some("http://127.0.0.1:0/v1".to_owned()),
            Duration::from_secs(1),
            &Version::new(0, 34, 4),
            &mut |n| panic!("{n:?}"),
        );
        assert_eq!(result, Ok(()));
    }

    /// A server that accepts and never answers costs one bounded wait, and
    /// the run goes on.
    #[test]
    fn a_silent_server_is_unknown_after_the_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}/v1", listener.local_addr().expect("addr"));
        let routes = [spec("ollama", "m")];
        let mut seen = Vec::new();
        let started = Instant::now();
        let result = check_ollama_versions(
            &routes,
            &|_| false,
            &|_| Some(url.clone()),
            Duration::from_millis(400),
            &Version::new(0, 34, 4),
            &mut |n| seen.push(n),
        );
        assert_eq!(result, Ok(()));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(matches!(&seen[..], [Notice::LocalServer(s)] if s.contains("is unknown")));
        drop(listener);
    }
}
