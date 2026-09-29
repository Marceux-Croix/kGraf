//! MCP server configuration.
//!
//! **A configuration that exists is valid.** Fields are private; the only ways to
//! get an [`McpServerConfig`] are [`McpServerConfig::new`] and deserialization, and
//! both go through [`McpServerConfig::validate`] (serde `try_from`). Unknown keys are
//! refused, so a misspelled `timout_ms` is an error rather than a silently used
//! default (fail loud).
//!
//! **Names follow Claude Code's** (`claude_ai_Gmail`), so ingested `allowed-tools`
//! entries match. Two servers whose names differ only in case are refused together
//! by [`validate_all`]: their tools would be told apart by case alone, which a user
//! reading a transcript would not.
//!
//! **Plain HTTP only to loopback.** Hosted servers are allowed (0051), and headers
//! typically carry a bearer token. Sending that token over plain HTTP to another
//! host puts it on the network in clear, so `http://` is accepted only for
//! `localhost` and loopback addresses.
//!
//! **Header values are never stored.** A Streamable HTTP header names the
//! environment variable its value is read from at connect time
//! ([`HeaderFromEnv`]), plus an optional literal prefix such as `Bearer `. A
//! literal header value is refused at load, so a credential cannot sit in a config
//! file, a transcript or a backup of either. A variable that is not set when the
//! server connects is an error, not an absent header (fail loud).
//!
//! **Secrets stay out of output.** `Serialize` and `Debug` replace every stdio
//! environment value with [`REDACTED`], so a configuration written to a transcript,
//! a log or a trace carries names, not credentials. A redacted value read back in is
//! refused, so a redacted dump cannot be loaded as if it were a working config.
//! Headers need no redaction: they hold variable names only.
//!
//! OAuth for hosted servers lives in [`crate::oauth`].

use kgraf_tools::QualifiedName;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;
use std::path::PathBuf;

/// The default per-request timeout, in milliseconds.
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
/// What a secret value serializes as.
pub const REDACTED: &str = "<redacted>";

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// Header names the Streamable HTTP transport sets itself. A configured header
/// with one of these names would collide with the transport's own value (a
/// forged `Mcp-Session-Id` on `initialize`, a second `Accept`), so it is refused.
pub const RESERVED_HEADERS: &[&str] = &[
    "accept",
    "content-length",
    "content-type",
    "host",
    "last-event-id",
    "mcp-protocol-version",
    "mcp-session-id",
];

/// Header names that always carry a credential. A header with one of these names
/// is always a secret; `"secret": false` on it is refused.
const ALWAYS_SECRET_HEADERS: &[&str] = &["authorization", "cookie", "proxy-authorization"];

/// Words that mark a header or a URL query parameter as credential-like when
/// one of them is a whole word of the name (words split at anything that is not
/// a letter or digit: `api_key`, `X-Auth-Token`). Matched as words, not as
/// substrings, so `author` and `keyword` are ordinary names.
const CREDENTIAL_WORDS: &[&str] = &[
    "auth",
    "authorization",
    "cookie",
    "credential",
    "credentials",
    "key",
    "passwd",
    "password",
    "pwd",
    "secret",
    "session",
    "sessionid",
    "sig",
    "signature",
    "token",
];

/// Endings that mark a run-together word as credential-like (`apikey`,
/// `accesstoken`, `clientsecret`). An ending must follow at least one letter.
const CREDENTIAL_SUFFIXES: &[&str] = &["key", "password", "secret", "token"];

/// Whether `name` looks like it carries a credential. A header named like one
/// is a secret unless it says `"secret": false`; a query parameter named like
/// one is refused.
pub(crate) fn is_credential_like(name: &str) -> bool {
    name.to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .any(|word| {
            CREDENTIAL_WORDS.contains(&word)
                || CREDENTIAL_SUFFIXES
                    .iter()
                    .any(|suffix| word.len() > suffix.len() && word.ends_with(suffix))
        })
}

/// Where one HTTP header's value comes from: the environment variable `env`, read
/// when the server connects, after the literal `prefix` (for example `Bearer `).
/// The configuration holds the variable's name, never its value.
///
/// `secret` says whether the value is a credential. A secret value is removed from
/// everything the server sends back, so it must be at least
/// [`crate::MIN_SECRET_CHARS`] long: a short one (`2.0`) would be cut out of
/// ordinary replies. It defaults to `true` for credential-like names
/// (`Authorization`, `X-Api-Key`, ...) and must be written out for any other name.
///
/// ```json
/// {"Authorization": {"env": "GITHUB_MCP_TOKEN", "prefix": "Bearer "},
///  "X-Api-Version": {"env": "API_VERSION", "secret": false}}
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawHeader")]
pub struct HeaderFromEnv {
    env: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    prefix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    secret: Option<bool>,
}

impl HeaderFromEnv {
    /// A header whose whole value is the variable `env`.
    pub fn new(env: impl Into<String>) -> Self {
        Self {
            env: env.into(),
            prefix: String::new(),
            secret: None,
        }
    }

    /// Put `prefix` before the variable's value (for example `Bearer `).
    pub fn with_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Say whether the value is a credential (see the type docs).
    pub fn with_secret(mut self, secret: bool) -> Self {
        self.secret = Some(secret);
        self
    }

    /// The environment variable the value is read from.
    pub fn env(&self) -> &str {
        &self.env
    }

    /// The literal text sent before the variable's value.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Whether the header named `name` is a secret, or why that is not known.
    pub(crate) fn is_secret(&self, name: &str) -> Result<bool, &'static str> {
        let always = ALWAYS_SECRET_HEADERS
            .iter()
            .any(|h| name.eq_ignore_ascii_case(h));
        match self.secret {
            Some(false) if always => Err("this header always carries a credential; it cannot be \
                 marked \"secret\": false"),
            Some(secret) => Ok(secret),
            None if always || is_credential_like(name) => Ok(true),
            None => Err(
                "say whether this header carries a credential: add \"secret\": true \
                 (its value is removed from replies) or \"secret\": false",
            ),
        }
    }
}

/// A header value as written: an `{"env", "prefix", "secret"}` object, or a literal
/// string. The literal is parsed only so that it can be refused with a message that
/// says what to write instead; serde's own "did not match any variant" would not.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawHeader {
    FromEnv(RawHeaderFromEnv),
    Literal(
        #[expect(dead_code, reason = "matched only to refuse it; the value is a secret")] String,
    ),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHeaderFromEnv {
    env: String,
    #[serde(default)]
    prefix: String,
    #[serde(default)]
    secret: Option<bool>,
}

impl TryFrom<RawHeader> for HeaderFromEnv {
    type Error = &'static str;

    fn try_from(raw: RawHeader) -> Result<Self, Self::Error> {
        match raw {
            RawHeader::FromEnv(RawHeaderFromEnv {
                env,
                prefix,
                secret,
            }) => Ok(Self {
                env,
                prefix,
                secret,
            }),
            RawHeader::Literal(_) => Err(
                "a header value must come from an environment variable: write \
                 {\"env\": \"VARIABLE\", \"prefix\": \"Bearer \"}, not the value itself",
            ),
        }
    }
}

/// How to reach a server.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransport {
    /// A local process speaking MCP on stdin and stdout.
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<PathBuf>,
    },
    /// A server over Streamable HTTP. Header values come from the environment.
    StreamableHttp {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, HeaderFromEnv>,
        /// The HTTP proxy to use. `HTTP_PROXY` and similar variables are ignored:
        /// a proxy sees every header, so it is used only when named here.
        #[serde(default)]
        proxy: Option<String>,
        /// The server authorizes with OAuth. Only then does a 401 challenge start
        /// the OAuth flow; otherwise it is an error.
        #[serde(default)]
        oauth: bool,
    },
}

impl McpTransport {
    /// A copy with every stdio environment value replaced by [`REDACTED`]. An HTTP
    /// transport holds no secret (header variable names only), so it is copied.
    fn redacted(&self) -> Self {
        match self {
            McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => McpTransport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: env
                    .keys()
                    .map(|k| (k.clone(), REDACTED.to_owned()))
                    .collect(),
                cwd: cwd.clone(),
            },
            http @ McpTransport::StreamableHttp { .. } => http.clone(),
        }
    }
}

/// Serialized form, secrets redacted.
impl Serialize for McpTransport {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(tag = "type", rename_all = "snake_case")]
        enum Out<'a> {
            Stdio {
                command: &'a str,
                args: &'a [String],
                env: &'a BTreeMap<String, String>,
                #[serde(skip_serializing_if = "Option::is_none")]
                cwd: &'a Option<PathBuf>,
            },
            StreamableHttp {
                url: &'a str,
                headers: &'a BTreeMap<String, HeaderFromEnv>,
                #[serde(skip_serializing_if = "Option::is_none")]
                proxy: &'a Option<String>,
                #[serde(skip_serializing_if = "std::ops::Not::not")]
                oauth: bool,
            },
        }
        let redacted = self.redacted();
        let out = match &redacted {
            McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => Out::Stdio {
                command,
                args,
                env,
                cwd,
            },
            McpTransport::StreamableHttp {
                url,
                headers,
                proxy,
                oauth,
            } => Out::StreamableHttp {
                url,
                headers,
                proxy,
                oauth: *oauth,
            },
        };
        out.serialize(serializer)
    }
}

impl fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.redacted() {
            McpTransport::Stdio {
                command,
                args,
                env,
                cwd,
            } => f
                .debug_struct("Stdio")
                .field("command", &command)
                .field("args", &args)
                .field("env", &env)
                .field("cwd", &cwd)
                .finish(),
            McpTransport::StreamableHttp {
                url,
                headers,
                proxy,
                oauth,
            } => f
                .debug_struct("StreamableHttp")
                .field("url", &url)
                .field("headers", &headers)
                .field("proxy", &proxy)
                .field("oauth", &oauth)
                .finish(),
        }
    }
}

/// The configuration as written, before validation.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    name: String,
    transport: McpTransport,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    instructions: Option<String>,
}

/// One MCP server, validated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawConfig")]
pub struct McpServerConfig {
    name: String,
    transport: McpTransport,
    timeout_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
}

impl TryFrom<RawConfig> for McpServerConfig {
    type Error = McpConfigError;

    fn try_from(raw: RawConfig) -> Result<Self, Self::Error> {
        let config = Self {
            name: raw.name,
            transport: raw.transport,
            timeout_ms: raw.timeout_ms,
            instructions: raw.instructions,
        };
        config.validate()?;
        Ok(config)
    }
}

/// A configuration that cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    /// The text is not a valid configuration document, or fails validation.
    #[error("cannot parse MCP server configuration")]
    Parse(#[from] serde_json::Error),

    /// The server name breaks the naming rules.
    #[error("invalid MCP server name")]
    Name(#[source] kgraf_tools::RegistryError),

    /// Two servers share a name, ignoring case.
    #[error("two MCP servers are named `{name}` (names are compared ignoring case)")]
    DuplicateServer { name: String },

    /// A stdio server has no command.
    #[error("MCP server `{server}` has an empty command")]
    EmptyCommand { server: String },

    /// An environment variable cannot be passed to a process.
    #[error("MCP server `{server}` has an invalid environment variable `{name}`: {reason}")]
    BadEnv {
        server: String,
        name: String,
        reason: String,
    },

    /// The URL cannot be used.
    #[error("MCP server `{server}` has an unusable URL `{url}`: {reason}")]
    BadUrl {
        server: String,
        url: String,
        reason: String,
    },

    /// A header cannot be sent.
    #[error("MCP server `{server}` has an invalid header `{name}`: {reason}")]
    BadHeader {
        server: String,
        name: String,
        reason: String,
    },

    /// The timeout is zero.
    #[error("MCP server `{server}` has a zero timeout")]
    ZeroTimeout { server: String },
}

impl McpServerConfig {
    /// A validated configuration with the default timeout and no instructions.
    pub fn new(name: impl Into<String>, transport: McpTransport) -> Result<Self, McpConfigError> {
        let config = Self {
            name: name.into(),
            transport,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            instructions: None,
        };
        config.validate()?;
        Ok(config)
    }

    /// Set the per-request timeout. Zero is an error.
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Result<Self, McpConfigError> {
        self.timeout_ms = timeout_ms;
        self.validate()?;
        Ok(self)
    }

    /// Set the user's instructions for this server's tools. They take precedence
    /// over the instructions the server reports at initialize.
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = Some(instructions.into());
        self
    }

    /// Parse one server from JSON; validation runs as part of deserialization.
    pub fn parse_json(text: &str) -> Result<Self, McpConfigError> {
        Ok(serde_json::from_str(text)?)
    }

    /// The server's name, as used in `mcp__<server>__<tool>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How to reach it. Includes secrets; do not log.
    pub fn transport(&self) -> &McpTransport {
        &self.transport
    }

    /// Per-request timeout in milliseconds.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// The user's instructions, if any.
    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }

    fn validate(&self) -> Result<(), McpConfigError> {
        let server = || self.name.clone();
        QualifiedName::check_server(&self.name).map_err(McpConfigError::Name)?;
        if self.timeout_ms == 0 {
            return Err(McpConfigError::ZeroTimeout { server: server() });
        }
        match &self.transport {
            McpTransport::Stdio { command, env, .. } => {
                if command.trim().is_empty() {
                    return Err(McpConfigError::EmptyCommand { server: server() });
                }
                for (name, value) in env {
                    let reason = if name.is_empty() || name.contains(['=', '\0']) {
                        Some("the name is empty or contains `=` or NUL")
                    } else if value.contains('\0') {
                        Some("the value contains NUL")
                    } else if value == REDACTED {
                        Some("the value is a redaction placeholder, not a secret")
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        return Err(McpConfigError::BadEnv {
                            server: server(),
                            name: name.clone(),
                            reason: reason.to_owned(),
                        });
                    }
                }
            }
            McpTransport::StreamableHttp {
                url,
                headers,
                proxy,
                oauth,
            } => {
                check_url(&self.name, url)?;
                for (name, value) in headers {
                    check_header(&self.name, name, value)?;
                    if *oauth && name.eq_ignore_ascii_case("authorization") {
                        return Err(McpConfigError::BadHeader {
                            server: server(),
                            name: name.clone(),
                            reason: "the OAuth token is sent as Authorization; remove this \
                                     header or set \"oauth\": false"
                                .to_owned(),
                        });
                    }
                }
                if let Some(proxy) = proxy {
                    check_proxy(&self.name, url, proxy)?;
                }
            }
        }
        Ok(())
    }

    /// The HTTP proxy this server is reached through, if one is configured.
    pub fn proxy(&self) -> Option<&str> {
        match &self.transport {
            McpTransport::StreamableHttp { proxy, .. } => proxy.as_deref(),
            McpTransport::Stdio { .. } => None,
        }
    }

    /// Whether this server authorizes with OAuth.
    pub fn uses_oauth(&self) -> bool {
        matches!(
            &self.transport,
            McpTransport::StreamableHttp { oauth: true, .. }
        )
    }
}

/// Validate a set of servers together: no two may share a name, ignoring case.
pub fn validate_all(configs: &[McpServerConfig]) -> Result<(), McpConfigError> {
    let mut seen = BTreeSet::new();
    for config in configs {
        if !seen.insert(config.name.to_lowercase()) {
            return Err(McpConfigError::DuplicateServer {
                name: config.name.clone(),
            });
        }
    }
    Ok(())
}

/// `raw` with every query value, the fragment and any userinfo replaced by
/// [`REDACTED`], for an error message. Text that does not parse as a URL is
/// replaced whole, because what it holds is unknown.
pub(crate) fn redact_url(raw: &str) -> String {
    let Ok(mut url) = url::Url::parse(raw) else {
        return REDACTED.to_owned();
    };
    if !url.username().is_empty() {
        let _ = url.set_username(REDACTED);
    }
    if url.password().is_some() {
        let _ = url.set_password(Some(REDACTED));
    }
    if url.query().is_some() {
        let names: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
        url.query_pairs_mut()
            .clear()
            .extend_pairs(names.iter().map(|k| (k.as_str(), REDACTED)));
    }
    if url.fragment().is_some() {
        url.set_fragment(Some(REDACTED));
    }
    url.to_string()
}

fn check_url(server: &str, raw: &str) -> Result<(), McpConfigError> {
    let bad = |reason: &str| McpConfigError::BadUrl {
        server: server.to_owned(),
        url: redact_url(raw),
        reason: reason.to_owned(),
    };
    let url = url::Url::parse(raw).map_err(|e| bad(&e.to_string()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("credentials belong in headers, not in the URL"));
    }
    // A URL is sent in clear to proxies and written to logs by servers and by
    // `reqwest`'s own errors, so a credential in its query string would leak.
    if let Some((name, _)) = url.query_pairs().find(|(k, _)| is_credential_like(k)) {
        return Err(bad(&format!(
            "the query parameter `{name}` looks like a credential; send it in a header \
             read from an environment variable"
        )));
    }
    match url.scheme() {
        "https" => Ok(()),
        "http" if is_loopback(&url) => Ok(()),
        "http" => Err(bad("plain http is allowed only to a loopback host")),
        _ => Err(bad("the scheme must be http or https")),
    }
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
        Some(url::Host::Ipv6(ip)) => IpAddr::V6(ip).is_loopback(),
        None => false,
    }
}

/// An HTTP header name is a token (RFC 9110) that the transport does not set
/// itself; the variable name must be one a process environment can hold; the
/// prefix may not break the line.
fn check_header(server: &str, name: &str, source: &HeaderFromEnv) -> Result<(), McpConfigError> {
    let bad = |reason: &str| McpConfigError::BadHeader {
        server: server.to_owned(),
        name: name.to_owned(),
        reason: reason.to_owned(),
    };
    let token_char = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+-.^_`|~".contains(c);
    if name.is_empty() || !name.chars().all(token_char) {
        return Err(bad("a header name must be a nonempty HTTP token"));
    }
    if RESERVED_HEADERS
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
    {
        return Err(bad("the Streamable HTTP transport sets this header itself"));
    }
    if source.env.is_empty() || source.env.contains(['=', '\0']) {
        return Err(bad(
            "the environment variable name is empty or contains `=` or NUL",
        ));
    }
    if source.prefix.contains(['\r', '\n', '\0']) {
        return Err(bad("the prefix may not contain CR, LF or NUL"));
    }
    source.is_secret(name).map_err(bad)?;
    Ok(())
}

/// A proxy is an http or https URL with no credentials in it, and only for an
/// https server: a plain-http server is on loopback, where a proxy has no use and
/// would read the traffic in clear.
fn check_proxy(server: &str, server_url: &str, raw: &str) -> Result<(), McpConfigError> {
    let bad = |reason: &str| McpConfigError::BadUrl {
        server: server.to_owned(),
        url: redact_url(raw),
        reason: format!("proxy: {reason}"),
    };
    let url = url::Url::parse(raw).map_err(|e| bad(&e.to_string()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(bad("proxy credentials in the URL are not supported"));
    }
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad("the scheme must be http or https"));
    }
    if url::Url::parse(server_url).is_ok_and(|u| u.scheme() != "https") {
        return Err(bad("a proxy is allowed only for an https server"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn http_transport(url: &str, headers: &[(&str, &str)]) -> McpTransport {
        McpTransport::StreamableHttp {
            url: url.into(),
            proxy: None,
            oauth: false,
            headers: headers
                .iter()
                .map(|(k, var)| ((*k).to_owned(), HeaderFromEnv::new(*var)))
                .collect(),
        }
    }

    fn http(name: &str, url: &str) -> Result<McpServerConfig, McpConfigError> {
        McpServerConfig::new(name, http_transport(url, &[]))
    }

    fn stdio(command: &str, env: &[(&str, &str)]) -> Result<McpServerConfig, McpConfigError> {
        McpServerConfig::new(
            "s",
            McpTransport::Stdio {
                command: command.into(),
                args: vec![],
                env: env
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                cwd: None,
            },
        )
    }

    #[test]
    fn a_stdio_server_parses_with_defaults() {
        let config = McpServerConfig::parse_json(
            r#"{"name": "fs", "transport": {"type": "stdio", "command": "mcp-fs", "args": ["--root", "."]}}"#,
        )
        .expect("valid");
        assert_eq!(config.timeout_ms(), DEFAULT_TIMEOUT_MS);
        assert!(matches!(config.transport(), McpTransport::Stdio { args, .. } if args.len() == 2));
    }

    #[test]
    fn an_http_server_parses_with_headers_and_instructions() {
        let config = McpServerConfig::parse_json(
            r#"{"name": "github", "timeout_ms": 5000, "instructions": "use sparingly",
                "transport": {"type": "streamable_http", "url": "https://mcp.example.com/mcp",
                              "headers": {"Authorization": {"env": "GH_TOKEN", "prefix": "Bearer "},
                                          "X-Team": {"env": "TEAM_ID", "secret": false}}}}"#,
        )
        .expect("valid");
        assert_eq!(config.timeout_ms(), 5000);
        assert_eq!(config.instructions(), Some("use sparingly"));
        let McpTransport::StreamableHttp { headers, .. } = config.transport() else {
            panic!("an http transport");
        };
        assert_eq!(headers["Authorization"].env(), "GH_TOKEN");
        assert_eq!(headers["Authorization"].prefix(), "Bearer ");
        assert_eq!(headers["X-Team"].prefix(), "");
    }

    #[test]
    fn a_literal_header_value_is_refused_and_the_error_says_what_to_write() {
        let error = McpServerConfig::parse_json(
            r#"{"name": "github", "transport": {"type": "streamable_http",
                "url": "https://mcp.example.com/mcp",
                "headers": {"Authorization": "Bearer topsecret"}}}"#,
        )
        .expect_err("a literal header value must be refused");
        let McpConfigError::Parse(inner) = &error else {
            panic!("a parse error, got {error:?}");
        };
        let text = inner.to_string();
        assert!(text.contains("environment variable"), "{text}");
        assert!(!text.contains("topsecret"), "{text}");
    }

    #[test]
    fn a_header_object_with_an_unknown_key_is_refused() {
        assert!(
            McpServerConfig::parse_json(
                r#"{"name": "s", "transport": {"type": "streamable_http", "url": "https://a.example",
                    "headers": {"X-Key": {"env": "K", "value": "v"}}}}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn deserialization_runs_validation() {
        // Without `try_from`, this would deserialize into a config with a zero
        // timeout and plain http to a remote host.
        for bad in [
            r#"{"name": "s", "timeout_ms": 0, "transport": {"type": "stdio", "command": "x"}}"#,
            r#"{"name": "s", "transport": {"type": "streamable_http", "url": "http://mcp.example.com"}}"#,
            r#"{"name": "bad__name", "transport": {"type": "stdio", "command": "x"}}"#,
        ] {
            assert!(
                matches!(
                    McpServerConfig::parse_json(bad),
                    Err(McpConfigError::Parse(_))
                ),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn an_unknown_key_is_refused_not_ignored() {
        assert!(
            McpServerConfig::parse_json(
                r#"{"name": "fs", "timout_ms": 5, "transport": {"type": "stdio", "command": "x"}}"#,
            )
            .is_err()
        );
        assert!(
            McpServerConfig::parse_json(
                r#"{"name": "fs", "transport": {"type": "stdio", "command": "x", "url": "https://a"}}"#,
            )
            .is_err()
        );
    }

    #[test]
    fn serialization_redacts_stdio_env_and_a_redacted_dump_does_not_load() {
        let env = stdio("x", &[("TOKEN", "hunter2")]).expect("valid");
        let json = serde_json::to_string(&env).expect("ser");
        assert!(!json.contains("hunter2"), "{json}");
        assert!(json.contains(REDACTED));
        assert!(!format!("{env:?}").contains("hunter2"));
        assert!(McpServerConfig::parse_json(&json).is_err());
    }

    #[test]
    fn an_http_config_round_trips_because_it_holds_no_secret() {
        let config = McpServerConfig::new(
            "github",
            McpTransport::StreamableHttp {
                url: "https://a.example".into(),
                proxy: None,
                oauth: false,
                headers: [(
                    "Authorization".to_owned(),
                    HeaderFromEnv::new("GH_TOKEN").with_prefix("Bearer "),
                )]
                .into(),
            },
        )
        .expect("valid");
        let json = serde_json::to_string(&config).expect("ser");
        assert!(json.contains("GH_TOKEN"), "{json}");
        assert_eq!(McpServerConfig::parse_json(&json).expect("reload"), config);
    }

    #[test]
    fn plain_http_is_refused_except_to_loopback() {
        for ok in [
            "http://localhost:8080/mcp",
            "http://127.0.0.1:3000",
            "http://[::1]:3000/mcp",
            "https://mcp.example.com",
        ] {
            http("s", ok).expect(ok);
        }
        for refused in [
            "http://mcp.example.com",
            "http://10.0.0.5/mcp",
            "ftp://localhost/x",
            "https://user:pw@mcp.example.com",
            "not a url",
        ] {
            assert!(
                matches!(http("s", refused), Err(McpConfigError::BadUrl { .. })),
                "{refused} must be refused"
            );
        }
    }

    #[test]
    fn header_injection_bad_variables_and_reserved_names_are_refused() {
        let with = |name: &str, source: HeaderFromEnv| {
            McpServerConfig::new(
                "s",
                McpTransport::StreamableHttp {
                    url: "https://a.example".into(),
                    proxy: None,
                    oauth: false,
                    headers: [(name.to_owned(), source)].into(),
                },
            )
        };
        for (name, source) in [
            (
                "X-Key",
                HeaderFromEnv::new("K").with_prefix("v\r\nX-Forged: 1"),
            ),
            ("X-Key", HeaderFromEnv::new("")),
            ("X-Key", HeaderFromEnv::new("A=B")),
            ("Mcp-Session-Id", HeaderFromEnv::new("K")),
            ("accept", HeaderFromEnv::new("K")),
        ] {
            assert!(
                matches!(with(name, source), Err(McpConfigError::BadHeader { .. })),
                "{name} must be refused"
            );
        }
        with("X-Key", HeaderFromEnv::new("K")).expect("an ordinary header is valid");
        assert!(matches!(
            McpServerConfig::new(
                "s",
                http_transport("https://a.example", &[("Bad Name", "v")])
            ),
            Err(McpConfigError::BadHeader { .. })
        ));
    }

    #[test]
    fn bad_names_commands_env_and_timeouts_are_refused() {
        assert!(matches!(
            http("bad__name", "https://a.example"),
            Err(McpConfigError::Name(_))
        ));
        assert!(matches!(
            http("s", "https://a.example").and_then(|c| c.with_timeout_ms(0)),
            Err(McpConfigError::ZeroTimeout { .. })
        ));
        assert!(matches!(
            stdio("  ", &[]),
            Err(McpConfigError::EmptyCommand { .. })
        ));
        assert!(matches!(
            stdio("x", &[("A=B", "v")]),
            Err(McpConfigError::BadEnv { .. })
        ));
        stdio("x", &[("TOKEN", "v")]).expect("valid");
    }

    fn http_with(
        url: &str,
        headers: &[(&str, HeaderFromEnv)],
        proxy: Option<&str>,
        oauth: bool,
    ) -> Result<McpServerConfig, McpConfigError> {
        McpServerConfig::new(
            "s",
            McpTransport::StreamableHttp {
                url: url.into(),
                headers: headers
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), v.clone()))
                    .collect(),
                proxy: proxy.map(str::to_owned),
                oauth,
            },
        )
    }

    /// Whether a header is a secret is known for every header: credential-like
    /// names default to secret, any other name must say, and Authorization cannot
    /// be declared public.
    #[test]
    fn every_header_says_whether_it_is_a_secret() {
        let url = "https://a.example";
        for name in ["Authorization", "X-Api-Key", "X-Auth-Token", "Cookie"] {
            http_with(url, &[(name, HeaderFromEnv::new("V"))], None, false).expect(name);
        }
        assert!(matches!(
            http_with(url, &[("X-Api-Version", HeaderFromEnv::new("V"))], None, false),
            Err(McpConfigError::BadHeader { ref reason, .. }) if reason.contains("secret")
        ));
        http_with(
            url,
            &[("X-Api-Version", HeaderFromEnv::new("V").with_secret(false))],
            None,
            false,
        )
        .expect("declared public");
        assert!(matches!(
            http_with(
                url,
                &[("Authorization", HeaderFromEnv::new("V").with_secret(false))],
                None,
                false
            ),
            Err(McpConfigError::BadHeader { .. })
        ));
        let parsed = McpServerConfig::parse_json(
            r#"{"name": "s", "transport": {"type": "streamable_http", "url": "https://a.example",
                "headers": {"X-Api-Version": {"env": "V", "secret": false}}}}"#,
        )
        .expect("the secret key parses");
        let json = serde_json::to_string(&parsed).expect("ser");
        assert!(json.contains(r#""secret":false"#), "{json}");
    }

    /// A credential in the query string is refused, and the error does not
    /// repeat it.
    #[test]
    fn a_credential_in_the_url_query_is_refused_without_being_repeated() {
        let error = http("s", "https://a.example/mcp?api_key=sk-LIVE-123&v=2")
            .expect_err("a key in the query must be refused");
        let text = error.to_string();
        assert!(text.contains("api_key"), "{text}");
        assert!(!text.contains("sk-LIVE-123"), "{text}");
        http("s", "https://a.example/mcp?profile=work").expect("an ordinary query is fine");
    }

    /// Credential names are matched as words or known endings, not as
    /// substrings, so ordinary parameters are not refused.
    #[test]
    fn credential_names_are_matched_as_words_not_substrings() {
        for credential in [
            "api_key",
            "access_token",
            "X-Amz-Signature",
            "sig",
            "apikey",
            "clientSecret",
            "X-Auth-Token",
            "session",
        ] {
            assert!(is_credential_like(credential), "{credential}");
        }
        for ordinary in ["author", "keyword", "profile", "tokenizer", "X-Api-Version"] {
            assert!(!is_credential_like(ordinary), "{ordinary}");
        }
        for query in ["?author=ann", "?keyword=rust", "?tokenizer=bpe"] {
            http("s", &format!("https://a.example/mcp{query}")).expect(query);
        }
    }

    #[test]
    fn a_proxy_is_an_https_server_option_without_credentials() {
        let url = "https://a.example";
        http_with(url, &[], Some("http://proxy.corp:3128"), false).expect("valid proxy");
        for (server_url, proxy) in [
            (url, "http://user:pw@proxy.corp:3128"),
            (url, "socks5://proxy.corp:1080"),
            ("http://127.0.0.1:3000/mcp", "http://proxy.corp:3128"),
        ] {
            assert!(
                matches!(
                    http_with(server_url, &[], Some(proxy), false),
                    Err(McpConfigError::BadUrl { .. })
                ),
                "{proxy} for {server_url} must be refused"
            );
        }
    }

    /// An OAuth server gets its token as Authorization, so a configured
    /// Authorization header would be dropped or duplicated; it is refused.
    #[test]
    fn an_oauth_server_cannot_also_configure_authorization() {
        let url = "https://a.example";
        assert!(matches!(
            http_with(
                url,
                &[("Authorization", HeaderFromEnv::new("V"))],
                None,
                true
            ),
            Err(McpConfigError::BadHeader { .. })
        ));
        let config = http_with(url, &[("X-Api-Key", HeaderFromEnv::new("V"))], None, true)
            .expect("other headers are kept on the OAuth path");
        assert!(config.uses_oauth());
    }

    #[test]
    fn claude_code_server_names_are_accepted() {
        http("claude_ai_Gmail", "https://a.example").expect("uppercase and underscores");
    }

    #[test]
    fn server_names_that_differ_only_in_case_collide() {
        let a = http("github", "https://a.example").expect("a");
        let b = http("GitHub", "https://b.example").expect("b");
        assert!(matches!(
            validate_all(&[a.clone(), b]),
            Err(McpConfigError::DuplicateServer { .. })
        ));
        validate_all(&[a, http("gitlab", "https://b.example").expect("c")]).expect("distinct");
    }
}
